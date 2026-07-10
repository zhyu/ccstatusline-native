use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    created_ms: u128,
    head_mtime_ns: Option<u128>,
    index_mtime_ns: Option<u128>,
    branch: Option<String>,
}

pub struct GitResolver {
    ttl_seconds: f64,
    memory: HashMap<PathBuf, Option<String>>,
}

impl GitResolver {
    pub fn new(ttl_seconds: f64) -> Self {
        Self {
            ttl_seconds: ttl_seconds.clamp(0.0, 60.0),
            memory: HashMap::new(),
        }
    }

    pub fn branch(&mut self, cwd: &Path) -> Option<String> {
        let cwd = cwd.to_path_buf();
        if let Some(value) = self.memory.get(&cwd) {
            return value.clone();
        }
        let value = self.branch_uncached(&cwd);
        self.memory.insert(cwd, value.clone());
        value
    }

    fn branch_uncached(&self, cwd: &Path) -> Option<String> {
        let git_dir = discover_git_dir(cwd)?;
        let head_mtime_ns = mtime_ns(&git_dir.join("HEAD"));
        let index_mtime_ns = mtime_ns(&git_dir.join("index"));
        let cache_path = cache_path(&git_dir, cwd);
        if let Ok(bytes) = fs::read(&cache_path) {
            if let Ok(entry) = serde_json::from_slice::<CacheEntry>(&bytes) {
                if entry.head_mtime_ns == head_mtime_ns
                    && entry.index_mtime_ns == index_mtime_ns
                    && self.is_fresh(entry.created_ms)
                {
                    return entry.branch;
                }
            }
        }

        let inside = run_git(cwd, &["rev-parse", "--is-inside-work-tree"])
            .is_some_and(|output| output == "true");
        let branch = inside
            .then(|| run_git(cwd, &["symbolic-ref", "--short", "HEAD"]))
            .flatten()
            .filter(|branch| !branch.is_empty());
        let entry = CacheEntry {
            created_ms: now_ms(),
            head_mtime_ns,
            index_mtime_ns,
            branch: branch.clone(),
        };
        let _ = write_cache(&cache_path, &entry);
        branch
    }

    fn is_fresh(&self, created_ms: u128) -> bool {
        if self.ttl_seconds == 0.0 {
            return true;
        }
        let age_ms = now_ms().saturating_sub(created_ms);
        age_ms as f64 <= self.ttl_seconds * 1000.0
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
    )
}

fn discover_git_dir(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd;
    loop {
        let dot_git = current.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            let value = fs::read_to_string(&dot_git).ok()?;
            let target = value.trim().strip_prefix("gitdir:")?.trim();
            let path = PathBuf::from(target);
            return Some(if path.is_absolute() {
                path
            } else {
                current.join(path)
            });
        }
        current = current.parent()?;
    }
}

fn cache_path(git_dir: &Path, cwd: &Path) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(git_dir.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(cwd.as_os_str().as_encoded_bytes());
    let key = format!("{:x}", hash.finalize());
    cache_root().join(format!("{key}.json"))
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

fn write_cache(path: &Path, entry: &CacheEntry) -> std::io::Result<()> {
    let parent = path.parent().expect("cache path has parent");
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".{}.{}.tmp", std::process::id(), now_ms()));
    fs::write(&temp, serde_json::to_vec(entry)?)?;
    fs::rename(temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_branch_and_invalidates_on_head_change() {
        let temp = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(temp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let mut resolver = GitResolver::new(5.0);
        assert_eq!(resolver.branch(temp.path()).as_deref(), Some("main"));
    }
}
