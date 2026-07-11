use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const GIT_SUMMARY_COMMAND: &str = "ccstatusline-native --git-summary";

const SUMMARY_CACHE_VERSION: u8 = 1;
const GIT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("cannot start git: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("cannot poll git: {0}")]
    Poll(#[source] std::io::Error),
    #[error("cannot read git output: {0}")]
    Read(#[source] std::io::Error),
    #[error("git output reader stopped unexpectedly")]
    ReaderThread,
    #[error("git status timed out")]
    Timeout,
    #[error("git exited unsuccessfully{0}")]
    Exit(String),
}

impl GitError {
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitSnapshot {
    branch: Option<String>,
    oid: Option<String>,
    ahead: u64,
    behind: u64,
    stashes: u64,
    conflicts: u64,
    staged: u64,
    unstaged: u64,
    untracked: u64,
    #[serde(skip)]
    action: Option<String>,
}

impl GitSnapshot {
    pub fn compact(&self) -> String {
        let mut parts = Vec::new();
        if let Some(branch) = &self.branch {
            parts.push(format!("⎇ {branch}"));
        } else if let Some(oid) = &self.oid {
            parts.push(format!("@{}", &oid[..oid.len().min(8)]));
        }
        if let Some(action) = &self.action {
            parts.push(action.clone());
        }
        for (count, symbol) in [
            (self.conflicts, "~"),
            (self.staged, "+"),
            (self.unstaged, "!"),
            (self.untracked, "?"),
            (self.ahead, "⇡"),
            (self.behind, "⇣"),
            (self.stashes, "*"),
        ] {
            if count > 0 {
                parts.push(format!("{symbol}{count}"));
            }
        }
        parts.join(" ")
    }

    fn with_action(mut self, git_dir: &Path) -> Self {
        self.action = repository_action(git_dir).map(str::to_owned);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SummaryCacheEntry {
    version: u8,
    created_ms: u128,
    head_mtime_ns: Option<u128>,
    index_mtime_ns: Option<u128>,
    snapshot: GitSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RepositoryKey {
    root: PathBuf,
    git_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct Repository {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

impl Repository {
    fn key(&self) -> RepositoryKey {
        RepositoryKey {
            root: self.root.clone(),
            git_dir: self.git_dir.clone(),
        }
    }
}

pub struct GitResolver {
    ttl_seconds: f64,
    cache_root: PathBuf,
    branch_memory: HashMap<PathBuf, Option<String>>,
    summary_memory: HashMap<RepositoryKey, GitSnapshot>,
}

impl GitResolver {
    pub fn new(ttl_seconds: f64) -> Self {
        Self::with_cache_root(ttl_seconds, cache_root())
    }

    fn with_cache_root(ttl_seconds: f64, cache_root: PathBuf) -> Self {
        Self {
            ttl_seconds: ttl_seconds.clamp(0.0, 60.0),
            cache_root,
            branch_memory: HashMap::new(),
            summary_memory: HashMap::new(),
        }
    }

    pub fn branch(&mut self, cwd: &Path) -> Option<String> {
        let key = normalize(cwd);
        if let Some(value) = self.branch_memory.get(&key) {
            return value.clone();
        }
        let value = self.branch_uncached(cwd);
        self.branch_memory.insert(key, value.clone());
        value
    }

    pub fn summary(&mut self, cwd: &Path) -> Result<Option<GitSnapshot>, GitError> {
        let deadline = Instant::now() + GIT_TIMEOUT;
        let environment_override = has_git_environment_override();
        let Some(repository) = (if environment_override {
            discover_repository_with_git(cwd, deadline)?
        } else {
            discover_repository(cwd)
        }) else {
            ensure_before(deadline)?;
            return Ok(None);
        };

        let key = repository.key();
        let memory_snapshot = if environment_override {
            None
        } else {
            self.summary_memory.get(&key)
        };
        if let Some(snapshot) = memory_snapshot {
            let snapshot = snapshot.clone().with_action(&repository.git_dir);
            ensure_before(deadline)?;
            return Ok(Some(snapshot));
        }

        let snapshot = if environment_override {
            // Git resolves relative GIT_DIR/GIT_WORK_TREE/GIT_INDEX_FILE
            // values from the command's working directory. Discovery started
            // at the status cwd, so the status query must use that same base.
            query_snapshot(cwd, deadline)?
        } else {
            self.summary_cached(&repository, deadline)?
        };
        if !environment_override {
            self.summary_memory.insert(key, snapshot.clone());
        }
        let snapshot = snapshot.with_action(&repository.git_dir);
        ensure_before(deadline)?;
        Ok(Some(snapshot))
    }

    fn branch_uncached(&self, cwd: &Path) -> Option<String> {
        if has_git_environment_override() {
            return branch_with_git(cwd);
        }
        let repository = discover_repository(cwd)?;
        match branch_from_head(&repository) {
            HeadBranch::Branch(branch) => Some(branch),
            HeadBranch::Detached => None,
            HeadBranch::NeedsGit => branch_with_git(&repository.root),
        }
    }

    fn summary_cached(
        &self,
        repository: &Repository,
        deadline: Instant,
    ) -> Result<GitSnapshot, GitError> {
        let head_mtime_ns = mtime_ns(&repository.git_dir.join("HEAD"));
        let index_mtime_ns = mtime_ns(&repository.git_dir.join("index"));
        let cache_path = summary_cache_path(&self.cache_root, repository);
        let cached = fs::read(&cache_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SummaryCacheEntry>(&bytes).ok())
            .filter(|entry| {
                entry.version == SUMMARY_CACHE_VERSION
                    && entry.head_mtime_ns == head_mtime_ns
                    && entry.index_mtime_ns == index_mtime_ns
                    && self.is_fresh(entry.created_ms)
            });
        if let Some(entry) = cached {
            return Ok(entry.snapshot);
        }

        ensure_before(deadline)?;
        let snapshot = query_snapshot(&repository.root, deadline)?;
        let entry = SummaryCacheEntry {
            version: SUMMARY_CACHE_VERSION,
            created_ms: now_ms(),
            head_mtime_ns,
            index_mtime_ns,
            snapshot: snapshot.clone(),
        };
        let _ = write_cache(&cache_path, &entry);
        Ok(snapshot)
    }

    fn is_fresh(&self, created_ms: u128) -> bool {
        if self.ttl_seconds == 0.0 {
            return true;
        }
        let age_ms = now_ms().saturating_sub(created_ms);
        age_ms as f64 <= self.ttl_seconds * 1000.0
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HeadBranch {
    Branch(String),
    Detached,
    NeedsGit,
}

fn branch_from_head(repository: &Repository) -> HeadBranch {
    let head_path = repository.git_dir.join("HEAD");
    if fs::symlink_metadata(&head_path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return HeadBranch::NeedsGit;
    }
    let Ok(head) = fs::read(&head_path) else {
        return HeadBranch::NeedsGit;
    };
    let head = trim_ascii(&head);
    let Some(reference) = head.strip_prefix(b"ref: ") else {
        return HeadBranch::Detached;
    };
    let Some(short) = reference.strip_prefix(b"refs/heads/") else {
        return HeadBranch::NeedsGit;
    };
    if short.is_empty()
        || repository_uses_reftable(&repository.common_dir)
        || reference_is_symbolic_or_symlink(&repository.common_dir.join(bytes_path(reference)))
        || short_ref_is_ambiguous(&repository.common_dir, short, reference)
    {
        return HeadBranch::NeedsGit;
    }
    HeadBranch::Branch(String::from_utf8_lossy(short).into_owned())
}

fn branch_with_git(cwd: &Path) -> Option<String> {
    let inside = run_git(cwd, &["rev-parse", "--is-inside-work-tree"])
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| trim_ascii(&output.stdout) == b"true");
    if !inside {
        return None;
    }
    let output = run_git(cwd, &["symbolic-ref", "--short", "HEAD"]).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(trim_ascii(&output.stdout)).into_owned())
        .filter(|branch| !branch.is_empty())
}

fn query_snapshot(cwd: &Path, deadline: Instant) -> Result<GitSnapshot, GitError> {
    let output = run_git_until(
        cwd,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--ahead-behind",
            "--show-stash",
            "--untracked-files=normal",
            "-z",
        ],
        deadline,
    )?;
    if !output.status.success() {
        return Err(GitError::Exit(exit_suffix(output.status)));
    }
    Ok(parse_porcelain_v2(&output.stdout))
}

fn parse_porcelain_v2(output: &[u8]) -> GitSnapshot {
    let mut snapshot = GitSnapshot::default();
    let mut records = output.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            if value != b"(initial)" {
                snapshot.oid = Some(String::from_utf8_lossy(value).into_owned());
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.head ") {
            if value != b"(detached)" {
                snapshot.branch = Some(String::from_utf8_lossy(value).into_owned());
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.ab ") {
            for count in value.split(|byte| byte.is_ascii_whitespace()) {
                if let Some(value) = count.strip_prefix(b"+") {
                    snapshot.ahead = parse_count(value);
                } else if let Some(value) = count.strip_prefix(b"-") {
                    snapshot.behind = parse_count(value);
                }
            }
        } else if let Some(value) = record.strip_prefix(b"# stash ") {
            snapshot.stashes = parse_count(value);
        } else if record.starts_with(b"? ") {
            snapshot.untracked += 1;
        } else if record.starts_with(b"u ") {
            snapshot.conflicts += 1;
        } else if record.starts_with(b"1 ") || record.starts_with(b"2 ") {
            if record.get(2).is_some_and(|status| *status != b'.') {
                snapshot.staged += 1;
            }
            if record.get(3).is_some_and(|status| *status != b'.') {
                snapshot.unstaged += 1;
            }
            if record.starts_with(b"2 ") {
                // Type-2 rename/copy records always have a second NUL record
                // containing the original path. It can begin with `? `.
                records.next();
            }
        }
    }
    snapshot
}

fn parse_count(value: &[u8]) -> u64 {
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn discover_repository(cwd: &Path) -> Option<Repository> {
    let normalized_cwd = normalize(cwd);
    let mut cwd = if normalized_cwd.is_file() {
        normalized_cwd.parent()?
    } else if normalized_cwd.is_dir() {
        normalized_cwd.as_path()
    } else {
        return None;
    };
    loop {
        let dot_git = cwd.join(".git");
        if dot_git.is_dir() {
            let git_dir = normalize(&dot_git);
            return Some(Repository {
                root: normalize(cwd),
                common_dir: discover_common_dir(&git_dir),
                git_dir,
            });
        }
        if dot_git.is_file() {
            let bytes = fs::read(&dot_git).ok()?;
            let target = trim_ascii(bytes.strip_prefix(b"gitdir:")?);
            let target = bytes_path(target);
            let git_dir = if target.is_absolute() {
                normalize(&target)
            } else {
                normalize(&cwd.join(target))
            };
            return Some(Repository {
                root: normalize(cwd),
                common_dir: discover_common_dir(&git_dir),
                git_dir,
            });
        }
        cwd = cwd.parent()?;
    }
}

fn discover_repository_with_git(
    cwd: &Path,
    deadline: Instant,
) -> Result<Option<Repository>, GitError> {
    let output = run_git_until(
        cwd,
        &[
            "rev-parse",
            "--is-inside-work-tree",
            "--show-toplevel",
            "--absolute-git-dir",
            "--path-format=absolute",
            "--git-common-dir",
        ],
        deadline,
    )?;
    if !output.status.success() {
        return Ok(None);
    }
    let lines = output
        .stdout
        .split(|byte| *byte == b'\n')
        .map(trim_ascii)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() < 4 || lines[0] != b"true" {
        return Ok(None);
    }
    Ok(Some(Repository {
        root: normalize(&bytes_path(lines[1])),
        git_dir: normalize(&bytes_path(lines[2])),
        common_dir: normalize(&bytes_path(lines[3])),
    }))
}

fn discover_common_dir(git_dir: &Path) -> PathBuf {
    let commondir_path = git_dir.join("commondir");
    let Ok(bytes) = fs::read(commondir_path) else {
        return git_dir.to_path_buf();
    };
    let path = bytes_path(trim_ascii(&bytes));
    if path.is_absolute() {
        normalize(&path)
    } else {
        normalize(&git_dir.join(path))
    }
}

fn repository_action(git_dir: &Path) -> Option<&'static str> {
    [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
    ]
    .into_iter()
    .find_map(|(marker, label)| git_dir.join(marker).exists().then_some(label))
}

fn repository_uses_reftable(common_dir: &Path) -> bool {
    common_dir.join("reftable").exists()
        || fs::read_to_string(common_dir.join("config")).is_ok_and(|config| {
            config
                .to_ascii_lowercase()
                .contains("refstorage = reftable")
        })
}

fn reference_is_symbolic_or_symlink(path: &Path) -> bool {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return true;
    }
    fs::read(path).is_ok_and(|value| trim_ascii(&value).starts_with(b"ref: "))
}

fn short_ref_is_ambiguous(common_dir: &Path, short: &[u8], full_ref: &[u8]) -> bool {
    let short_path = bytes_path(short);
    let mut loose_candidates = vec![
        common_dir.join(&short_path),
        common_dir.join("refs").join(&short_path),
        common_dir.join("refs/tags").join(&short_path),
        common_dir.join("refs/remotes").join(&short_path),
    ];
    loose_candidates.push(
        common_dir
            .join("refs/remotes")
            .join(&short_path)
            .join("HEAD"),
    );
    for candidate in loose_candidates {
        if candidate.exists() && candidate != common_dir.join(bytes_path(full_ref)) {
            return true;
        }
    }

    let packed_candidates = [
        [b"refs/".as_slice(), short].concat(),
        [b"refs/tags/".as_slice(), short].concat(),
        [b"refs/remotes/".as_slice(), short].concat(),
        [b"refs/remotes/".as_slice(), short, b"/HEAD".as_slice()].concat(),
    ];
    let Ok(packed_refs) = fs::read(common_dir.join("packed-refs")) else {
        return false;
    };
    packed_refs.split(|byte| *byte == b'\n').any(|line| {
        let Some(space) = line.iter().position(|byte| *byte == b' ') else {
            return false;
        };
        let reference = &line[space + 1..];
        reference != full_ref
            && packed_candidates
                .iter()
                .any(|candidate| reference == candidate)
    })
}

struct GitOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
    run_git_until(cwd, args, Instant::now() + GIT_TIMEOUT)
}

fn run_git_until(cwd: &Path, args: &[&str], deadline: Instant) -> Result<GitOutput, GitError> {
    ensure_before(deadline)?;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(GitError::Spawn)?;
    let mut stdout = child.stdout.take().expect("piped stdout is available");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = sender.send(result);
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Err(GitError::Poll(error));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            thread::spawn(move || {
                let _ = child.wait();
            });
            // Do not join here: a grandchild that inherited stdout could keep
            // the pipe open after Git itself is killed. The detached reader
            // exits when that final descriptor closes, while our timeout stays
            // bounded.
            return Err(GitError::Timeout);
        }
        thread::sleep(Duration::from_millis(1));
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let stdout = match receiver.recv_timeout(remaining) {
        Ok(result) => result.map_err(GitError::Read)?,
        Err(mpsc::RecvTimeoutError::Timeout) => return Err(GitError::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => return Err(GitError::ReaderThread),
    };
    Ok(GitOutput { status, stdout })
}

fn ensure_before(deadline: Instant) -> Result<(), GitError> {
    (Instant::now() < deadline)
        .then_some(())
        .ok_or(GitError::Timeout)
}

fn has_git_environment_override() -> bool {
    const OVERRIDES: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_NAMESPACE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
    ];
    OVERRIDES.iter().any(|key| env::var_os(key).is_some())
        || env::vars_os().any(|(key, _)| {
            key.to_str().is_some_and(|key| {
                key == "GIT_CONFIG_COUNT"
                    || key.starts_with("GIT_CONFIG_KEY_")
                    || key.starts_with("GIT_CONFIG_VALUE_")
            })
        })
}

fn summary_cache_path(cache_root: &Path, repository: &Repository) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(b"rich-summary-v1\0");
    hash.update(repository.root.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(repository.git_dir.as_os_str().as_encoded_bytes());
    let key = format!("{:x}", hash.finalize());
    cache_root.join(format!("summary-{key}.json"))
}

fn cache_root() -> PathBuf {
    if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(path).join("ccstatusline-native/git");
    }
    let home = env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".cache/ccstatusline-native/git")
}

fn mtime_ns(path: &Path) -> Option<u128> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn write_cache(path: &Path, entry: &SummaryCacheEntry) -> std::io::Result<()> {
    let parent = path.parent().expect("cache path has parent");
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".{}.{}.tmp", std::process::id(), now_ms()));
    fs::write(&temp, serde_json::to_vec(entry)?)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(temp);
        return Err(error);
    }
    Ok(())
}

fn normalize(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(unix)]
fn bytes_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes.to_vec()).into()
}

#[cfg(not(unix))]
fn bytes_path(bytes: &[u8]) -> PathBuf {
    String::from_utf8_lossy(bytes).into_owned().into()
}

fn trim_ascii(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn exit_suffix(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| format!(" with code {code}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn init_repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q", "-b", "main"]);
        fs::write(temp.path().join("tracked.txt"), "base\n").unwrap();
        git(temp.path(), &["add", "tracked.txt"]);
        git(temp.path(), &["commit", "-qm", "initial"]);
        temp
    }

    #[test]
    fn parses_and_formats_the_agent_workflow_order() {
        let output = b"# branch.oid 0123456789abcdef\x00# branch.head feature/demo\x00# branch.upstream origin/feature/demo\x00# branch.ab +2 -1\x00# stash 3\x001 M. N... 100644 100644 100644 abc abc staged.txt\x001 .M N... 100644 100644 100644 abc abc modified.txt\x00? untracked.txt\x00u UU N... 100644 100644 100644 100644 abc abc abc conflict.txt\x00";
        let mut snapshot = parse_porcelain_v2(output);
        snapshot.action = Some("rebase".into());
        assert_eq!(
            snapshot.compact(),
            "⎇ feature/demo rebase ~1 +1 !1 ?1 ⇡2 ⇣1 *3"
        );
    }

    #[test]
    fn consumes_the_original_path_after_a_rename() {
        let output = b"# branch.oid abcdef0123456789\x00# branch.head main\x002 R. N... 100644 100644 100644 abc abc R100 renamed.txt\x00? original.txt\x00";
        let snapshot = parse_porcelain_v2(output);
        assert_eq!(snapshot.staged, 1);
        assert_eq!(snapshot.untracked, 0);
        assert_eq!(snapshot.compact(), "⎇ main +1");
    }

    #[test]
    fn counts_partial_changes_and_keeps_conflicts_exclusive() {
        let output = b"# branch.oid abcdef0123456789\x00# branch.head main\x001 MM N... 100644 100644 100644 abc abc partial.txt\x00u UU N... 100644 100644 100644 100644 abc abc abc conflict.txt\x00";
        let snapshot = parse_porcelain_v2(output);
        assert_eq!(snapshot.staged, 1);
        assert_eq!(snapshot.unstaged, 1);
        assert_eq!(snapshot.conflicts, 1);
        assert_eq!(snapshot.compact(), "⎇ main ~1 +1 !1");
    }

    #[test]
    fn formats_detached_and_unborn_heads() {
        let detached =
            parse_porcelain_v2(b"# branch.oid abcdef0123456789\0# branch.head (detached)\0");
        assert_eq!(detached.compact(), "@abcdef01");
        let unborn = parse_porcelain_v2(b"# branch.oid (initial)\0# branch.head main\0");
        assert_eq!(unborn.compact(), "⎇ main");
    }

    #[test]
    fn reads_branch_without_git_and_handles_ambiguous_short_names() {
        let repo = init_repo();
        let cache = tempfile::tempdir().unwrap();
        let mut resolver = GitResolver::with_cache_root(5.0, cache.path().into());
        assert_eq!(resolver.branch(repo.path()).as_deref(), Some("main"));

        git(
            repo.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git(repo.path(), &["pack-refs", "--all"]);
        let repository = discover_repository(repo.path()).unwrap();
        assert_eq!(
            branch_from_head(&repository),
            HeadBranch::Branch("main".into())
        );

        git(repo.path(), &["tag", "main"]);
        let mut resolver = GitResolver::with_cache_root(5.0, cache.path().into());
        assert_eq!(resolver.branch(repo.path()).as_deref(), Some("heads/main"));
    }

    #[test]
    fn queries_real_dirty_state_and_reuses_the_disk_cache() {
        let repo = init_repo();
        fs::write(repo.path().join("tracked.txt"), "staged\n").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        fs::write(repo.path().join("tracked.txt"), "unstaged\n").unwrap();
        fs::write(repo.path().join("new.txt"), "new\n").unwrap();
        let cache = tempfile::tempdir().unwrap();

        let mut resolver = GitResolver::with_cache_root(5.0, cache.path().into());
        let snapshot = resolver.summary(repo.path()).unwrap().unwrap();
        assert_eq!(snapshot.compact(), "⎇ main +1 !1 ?1");

        let files = fs::read_dir(cache.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert!(files.iter().any(|name| {
            Path::new(name)
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("summary-"))
        }));

        // A zero TTL means no time expiry. A separate resolver proves the
        // on-disk snapshot is shared by one-shot status-line processes.
        fs::remove_file(repo.path().join("new.txt")).unwrap();
        let mut cached = GitResolver::with_cache_root(0.0, cache.path().into());
        assert_eq!(
            cached.summary(repo.path()).unwrap().unwrap().compact(),
            "⎇ main +1 !1 ?1"
        );
    }

    #[test]
    fn index_change_invalidates_even_an_unbounded_cache_entry() {
        let repo = init_repo();
        let cache = tempfile::tempdir().unwrap();
        let mut resolver = GitResolver::with_cache_root(0.0, cache.path().into());
        assert_eq!(
            resolver.summary(repo.path()).unwrap().unwrap().compact(),
            "⎇ main"
        );

        fs::write(repo.path().join("tracked.txt"), "changed\n").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        let mut resolver = GitResolver::with_cache_root(0.0, cache.path().into());
        assert_eq!(
            resolver.summary(repo.path()).unwrap().unwrap().compact(),
            "⎇ main +1"
        );
    }

    #[test]
    fn operation_markers_are_read_fresh_from_the_worktree_git_dir() {
        let repo = init_repo();
        let cache = tempfile::tempdir().unwrap();
        let repository = discover_repository(repo.path()).unwrap();
        let mut resolver = GitResolver::with_cache_root(0.0, cache.path().into());
        assert_eq!(
            resolver.summary(repo.path()).unwrap().unwrap().compact(),
            "⎇ main"
        );

        fs::write(repository.git_dir.join("MERGE_HEAD"), "test\n").unwrap();
        assert_eq!(
            resolver.summary(repo.path()).unwrap().unwrap().compact(),
            "⎇ main merge"
        );
        fs::create_dir(repository.git_dir.join("rebase-merge")).unwrap();
        assert_eq!(
            resolver.summary(repo.path()).unwrap().unwrap().compact(),
            "⎇ main rebase"
        );
    }

    #[test]
    fn recognizes_every_supported_operation_marker() {
        let git_dir = tempfile::tempdir().unwrap();
        for (marker, expected, directory) in [
            ("MERGE_HEAD", "merge", false),
            ("CHERRY_PICK_HEAD", "cherry-pick", false),
            ("REVERT_HEAD", "revert", false),
            ("BISECT_LOG", "bisect", false),
            ("rebase-apply", "rebase", true),
            ("rebase-merge", "rebase", true),
        ] {
            let path = git_dir.path().join(marker);
            if directory {
                fs::create_dir(&path).unwrap();
            } else {
                fs::write(&path, b"marker\n").unwrap();
            }
            assert_eq!(repository_action(git_dir.path()), Some(expected));
            if directory {
                fs::remove_dir(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
    }

    #[test]
    fn linked_worktree_uses_its_own_git_dir_for_actions() {
        let source = init_repo();
        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked");
        git(
            source.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/worktree",
                linked.to_str().unwrap(),
            ],
        );
        let repository = discover_repository(&linked).unwrap();
        assert_ne!(repository.git_dir, repository.common_dir);

        let cache = tempfile::tempdir().unwrap();
        let mut resolver = GitResolver::with_cache_root(5.0, cache.path().into());
        assert_eq!(
            resolver.summary(&linked).unwrap().unwrap().compact(),
            "⎇ feature/worktree"
        );
        fs::write(repository.git_dir.join("CHERRY_PICK_HEAD"), b"marker\n").unwrap();
        assert_eq!(
            resolver.summary(&linked).unwrap().unwrap().compact(),
            "⎇ feature/worktree cherry-pick"
        );
    }

    #[test]
    fn no_repository_is_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let mut resolver = GitResolver::with_cache_root(5.0, cache.path().into());
        assert!(resolver.summary(temp.path()).unwrap().is_none());
        assert_eq!(resolver.branch(temp.path()), None);
    }
}
