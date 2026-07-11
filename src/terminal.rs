use std::env;

pub fn width() -> Option<usize> {
    let override_width = env::var("CCSTATUSLINE_WIDTH").ok();
    let columns = env::var("COLUMNS").ok();
    resolve_width(
        override_width.as_deref(),
        columns.as_deref(),
        probe_terminal_width,
    )
}

fn resolve_width(
    override_width: Option<&str>,
    columns: Option<&str>,
    probe: impl FnOnce() -> Option<usize>,
) -> Option<usize> {
    override_width
        .and_then(parse_positive_decimal_prefix)
        .or_else(|| columns.and_then(parse_positive_decimal_prefix))
        .or_else(probe)
}

fn parse_positive_decimal_prefix(value: &str) -> Option<usize> {
    let value = value.trim_start();
    let (negative, digits_source) = match value.as_bytes().first() {
        Some(b'+') => (false, &value[1..]),
        Some(b'-') => (true, &value[1..]),
        _ => (false, value),
    };
    if negative {
        return None;
    }
    let digit_count = digits_source.bytes().take_while(u8::is_ascii_digit).count();
    digits_source[..digit_count]
        .parse::<usize>()
        .ok()
        .filter(|width| *width > 0)
}

#[cfg(unix)]
fn probe_terminal_width() -> Option<usize> {
    use std::os::fd::AsRawFd;

    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO, libc::STDIN_FILENO] {
        if let Some(width) = width_for_fd(fd) {
            return Some(width);
        }
    }

    if let Some(file) = open_terminal(std::path::Path::new("/dev/tty")) {
        if let Some(width) = width_for_fd(file.as_raw_fd()) {
            return Some(width);
        }
    }

    probe_ancestor_tty().or_else(probe_tput)
}

#[cfg(not(unix))]
fn probe_terminal_width() -> Option<usize> {
    None
}

#[cfg(unix)]
fn width_for_fd(fd: std::os::fd::RawFd) -> Option<usize> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: TIOCGWINSZ writes a winsize value to the valid, aligned pointer.
    // Failure (including a non-terminal file descriptor) is handled below.
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // SAFETY: a successful TIOCGWINSZ initialized the winsize value.
    let size = unsafe { size.assume_init() };
    let width = usize::from(size.ws_col);
    (width > 0).then_some(width)
}

#[cfg(unix)]
fn open_terminal(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let flags = libc::O_NOCTTY | libc::O_NONBLOCK;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .ok()
}

#[cfg(unix)]
fn probe_ancestor_tty() -> Option<usize> {
    use std::os::fd::AsRawFd;

    // SAFETY: getppid has no preconditions and does not access memory.
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..8 {
        if pid <= 0 {
            break;
        }
        let Some(process) = process_info(pid) else {
            break;
        };
        if let Some(tty) = process.tty.as_deref() {
            if let Some(path) = tty_device_path(tty) {
                if let Some(file) = open_terminal(&path) {
                    if let Some(width) = width_for_fd(file.as_raw_fd()) {
                        return Some(width);
                    }
                }
            }
        }
        if process.parent <= 0 || process.parent == pid {
            break;
        }
        pid = process.parent;
    }
    None
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct ProcessInfo {
    parent: libc::pid_t,
    tty: Option<String>,
}

#[cfg(unix)]
fn process_info(pid: libc::pid_t) -> Option<ProcessInfo> {
    use std::process::{Command, Stdio};

    let output = Command::new("ps")
        .args(["-o", "ppid=,tty=", "-p"])
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_process_info(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(unix)]
fn parse_process_info(output: &str) -> Option<ProcessInfo> {
    let mut fields = output.split_whitespace();
    let parent = fields.next()?.parse::<libc::pid_t>().ok()?;
    let tty = fields
        .next()
        .filter(|tty| *tty != "?" && *tty != "??")
        .map(ToOwned::to_owned);
    Some(ProcessInfo { parent, tty })
}

#[cfg(unix)]
fn tty_device_path(tty: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, Path};

    let relative = Path::new(tty);
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(Path::new("/dev").join(relative))
}

#[cfg(unix)]
fn probe_tput() -> Option<usize> {
    use std::process::{Command, Stdio};

    let output = Command::new("tput")
        .arg("cols")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout))
        .and_then(|width| parse_positive_decimal_prefix(&width))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_width_like_javascript_parse_int() {
        assert_eq!(parse_positive_decimal_prefix("120garbage"), Some(120));
        assert_eq!(parse_positive_decimal_prefix("  +80px"), Some(80));
        assert_eq!(parse_positive_decimal_prefix("0"), None);
        assert_eq!(parse_positive_decimal_prefix("-10"), None);
        assert_eq!(parse_positive_decimal_prefix("garbage"), None);
    }

    #[test]
    fn resolution_prefers_override_then_columns_then_probe() {
        assert_eq!(
            resolve_width(Some("140"), Some("120"), || Some(100)),
            Some(140)
        );
        assert_eq!(resolve_width(None, Some("120"), || Some(100)), Some(120));
        assert_eq!(resolve_width(None, None, || Some(100)), Some(100));
    }

    #[test]
    fn invalid_environment_values_fall_through() {
        assert_eq!(
            resolve_width(Some("wide"), Some("120"), || Some(100)),
            Some(120)
        );
        assert_eq!(
            resolve_width(Some("0"), Some("wide"), || Some(100)),
            Some(100)
        );
        assert_eq!(resolve_width(Some("-2"), None, || None), None);
    }

    #[cfg(unix)]
    #[test]
    fn parses_parent_and_tty_from_ps_output() {
        assert_eq!(
            parse_process_info("  1234 ttys009\n"),
            Some(ProcessInfo {
                parent: 1234,
                tty: Some("ttys009".into()),
            })
        );
        assert_eq!(
            parse_process_info("  42 ??\n"),
            Some(ProcessInfo {
                parent: 42,
                tty: None,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn accepts_only_relative_tty_device_names() {
        assert_eq!(tty_device_path("ttys001"), Some("/dev/ttys001".into()));
        assert_eq!(tty_device_path("pts/4"), Some("/dev/pts/4".into()));
        assert_eq!(tty_device_path("../../tmp/file"), None);
        assert_eq!(tty_device_path("/dev/ttys001"), None);
    }
}
