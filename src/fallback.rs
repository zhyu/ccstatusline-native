use std::env;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum FallbackError {
    #[error(
        "no compatible fallback found; install ccstatusline {version}, Bun (bunx), or npm (npx)"
    )]
    Missing { version: &'static str },
    #[error("cannot start fallback `{program}`: {source}")]
    Start {
        program: String,
        source: std::io::Error,
    },
    #[error("cannot write status JSON to fallback: {0}")]
    Stdin(std::io::Error),
    #[error("cannot wait for fallback: {0}")]
    Wait(std::io::Error),
}

#[derive(Debug, Clone)]
struct Runner {
    program: PathBuf,
    prefix_args: Vec<OsString>,
}

impl Runner {
    fn command(&self, config: &Path) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.prefix_args).arg("--config").arg(config);
        bridge_width_environment(&mut command, crate::terminal::width());
        command
    }
}

pub struct FallbackOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

pub fn render(config: &Path, stdin: &[u8]) -> Result<FallbackOutput, FallbackError> {
    let runner = select_runner()?;
    let mut command = runner.command(config);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let display = runner.program.display().to_string();
    let mut child = command.spawn().map_err(|source| FallbackError::Start {
        program: display,
        source,
    })?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin)
        .map_err(FallbackError::Stdin)?;
    let output = child.wait_with_output().map_err(FallbackError::Wait)?;
    Ok(FallbackOutput {
        status: output.status,
        stdout: output.stdout,
    })
}

pub fn tui(config: &Path) -> Result<ExitStatus, FallbackError> {
    let runner = select_runner()?;
    let mut command = runner.command(config);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command.status().map_err(|source| FallbackError::Start {
        program: runner.program.display().to_string(),
        source,
    })
}

fn select_runner() -> Result<Runner, FallbackError> {
    if let Some(program) = env::var_os("CCSTATUSLINE_NATIVE_FALLBACK") {
        return Ok(Runner {
            program: PathBuf::from(program),
            prefix_args: Vec::new(),
        });
    }

    if let Some(program) = find_program("ccstatusline") {
        if reference_version_matches(&program) {
            return Ok(Runner {
                program,
                prefix_args: Vec::new(),
            });
        }
    }
    if let Some(program) = find_program("bunx") {
        return Ok(Runner {
            program,
            prefix_args: vec![
                OsString::from("-y"),
                OsString::from(format!(
                    "ccstatusline@{}",
                    crate::REFERENCE_CCSTATUSLINE_VERSION
                )),
            ],
        });
    }
    if let Some(program) = find_program("npx") {
        return Ok(Runner {
            program,
            prefix_args: vec![
                OsString::from("--yes"),
                OsString::from(format!(
                    "ccstatusline@{}",
                    crate::REFERENCE_CCSTATUSLINE_VERSION
                )),
            ],
        });
    }
    Err(FallbackError::Missing {
        version: crate::REFERENCE_CCSTATUSLINE_VERSION,
    })
}

fn reference_version_matches(program: &Path) -> bool {
    Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .any(|part| part == crate::REFERENCE_CCSTATUSLINE_VERSION)
        })
}

fn find_program(name: impl AsRef<OsStr>) -> Option<PathBuf> {
    let name = name.as_ref();
    if Path::new(name).components().count() > 1 {
        return Path::new(name).is_file().then(|| PathBuf::from(name));
    }
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(name);
        candidate.is_file().then_some(candidate)
    })
}

fn bridge_width_environment(command: &mut Command, width: Option<usize>) {
    if let Some(width) = width {
        command.env("CCSTATUSLINE_WIDTH", width.to_string());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn package_version_is_pinned() {
        let package = format!("ccstatusline@{}", crate::REFERENCE_CCSTATUSLINE_VERSION);
        assert_eq!(package, "ccstatusline@2.2.22");
    }
}
