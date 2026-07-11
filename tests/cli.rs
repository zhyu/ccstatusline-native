#![cfg(unix)]

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn unsupported_config(directory: &std::path::Path) -> std::path::PathBuf {
    let mut config: Value = serde_json::from_str(include_str!("fixtures/settings.json")).unwrap();
    config["lines"][0][0]["type"] = Value::String("session-cost".into());
    let path = directory.join("settings.json");
    fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    path
}

fn executable_script(directory: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = directory.join("reference");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn pseudo_terminal(columns: u16) -> (fs::File, fs::File) {
    let mut master_fd = -1;
    let mut slave_fd = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let size_pointer = std::ptr::addr_of_mut!(size);
    // SAFETY: openpty receives valid output pointers, no terminal name buffer,
    // default attributes, and a fully initialized window size.
    let result = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            size_pointer,
        )
    };
    assert_eq!(
        result,
        0,
        "openpty failed: {}",
        std::io::Error::last_os_error()
    );

    // SAFETY: successful openpty returned two owned file descriptors.
    let master = unsafe { fs::File::from_raw_fd(master_fd) };
    // SAFETY: successful openpty returned two distinct owned file descriptors.
    let slave = unsafe { fs::File::from_raw_fd(slave_fd) };
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr initializes attributes for the valid PTY slave fd.
    let result = unsafe { libc::tcgetattr(slave.as_raw_fd(), attributes.as_mut_ptr()) };
    assert_eq!(
        result,
        0,
        "tcgetattr failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: successful tcgetattr initialized the termios value.
    let mut attributes = unsafe { attributes.assume_init() };
    // SAFETY: cfmakeraw mutates a valid termios value in place.
    unsafe { libc::cfmakeraw(&mut attributes) };
    // SAFETY: tcsetattr reads the initialized termios value for the valid fd.
    let result = unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attributes) };
    assert_eq!(
        result,
        0,
        "tcsetattr failed: {}",
        std::io::Error::last_os_error()
    );
    (master, slave)
}

fn read_pty(mut master: fs::File) -> Vec<u8> {
    // Keep reads from blocking while a parent-held slave descriptor keeps the
    // PTY buffer alive after the child exits.
    // SAFETY: fcntl operates on the valid, owned PTY master descriptor.
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(
        flags >= 0,
        "F_GETFL failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: F_SETFL updates only the status flags for the valid descriptor.
    let result =
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    assert_eq!(
        result,
        0,
        "F_SETFL failed: {}",
        std::io::Error::last_os_error()
    );

    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => output.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("cannot read PTY output: {error}"),
        }
    }
    output
}

#[test]
fn unsupported_config_replays_stdin_and_keeps_warning_off_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let config = unsupported_config(temp.path());
    let reference = executable_script(temp.path(), "cat");
    let input = b"{\"private_status_value\":\"must-not-be-logged\"}\n";

    let mut child = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_NATIVE_FALLBACK", reference)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, input);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("fast path disabled"));
    assert!(stderr.contains("--support-report"));
    assert!(!stderr.contains("must-not-be-logged"));
}

#[test]
fn fallback_receives_the_normalized_resolved_width() {
    let temp = tempfile::tempdir().unwrap();
    let config = unsupported_config(temp.path());
    let reference = executable_script(temp.path(), "printf %s \"$CCSTATUSLINE_WIDTH\"");

    let output = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_NATIVE_FALLBACK", reference)
        .env("CCSTATUSLINE_WIDTH", "wide")
        .env("COLUMNS", "131px")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, b"131");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("fast path disabled")
    );
}

#[test]
fn true_unknown_width_uses_native_reference_semantics() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("settings.json");
    fs::write(&config, include_bytes!("fixtures/settings.json")).unwrap();
    let reference = executable_script(temp.path(), "printf fallback");
    let input = include_bytes!("fixtures/status.json");

    let mut command = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"));
    command
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_NATIVE_FALLBACK", reference)
        .env("PATH", "/nonexistent")
        .env_remove("CCSTATUSLINE_WIDTH")
        .env_remove("COLUMNS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe and the closure performs no other
    // work between fork and exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout.len(), 767);
    // ccstatusline 2.2.23 with both width variables absent and PATH disabled,
    // which makes its ancestor, stty, tput, and Git probes unavailable.
    assert_eq!(
        format!("{:x}", Sha256::digest(&output.stdout)),
        "2dab8943d1022b07e015cdf3cbf88562accaa887db6e3bb6a82e20299276aa14"
    );
}

#[test]
fn null_live_context_uses_transcript_without_runtime_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("settings.json");
    let transcript = temp.path().join("session.jsonl");
    fs::write(&config, include_bytes!("fixtures/settings.json")).unwrap();
    fs::write(
        &transcript,
        [
            serde_json::json!({
                "timestamp": "2026-07-11T01:00:00.000Z",
                "message": {
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 40000,
                        "output_tokens": 9000,
                        "cache_read_input_tokens": 10000,
                        "cache_creation_input_tokens": 5000
                    }
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "system",
                "subtype": "compact_boundary",
                "timestamp": "2026-07-11T01:01:00.000Z"
            })
            .to_string(),
            serde_json::json!({
                "timestamp": "2026-07-11T01:02:00.000Z",
                "message": {
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 10000,
                        "output_tokens": 5000,
                        "cache_read_input_tokens": 3000,
                        "cache_creation_input_tokens": 2000
                    }
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .unwrap();
    let reference = executable_script(temp.path(), "exit 97");
    let mut status: Value = serde_json::from_slice(include_bytes!("fixtures/status.json")).unwrap();
    status["transcript_path"] = Value::String(transcript.to_string_lossy().into_owned());
    status["context_window"]["current_usage"] = Value::Null;
    status["context_window"]["used_percentage"] = Value::Null;
    status["context_window"]["remaining_percentage"] = Value::Null;

    let mut child = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_NATIVE_FALLBACK", reference)
        .env("CCSTATUSLINE_WIDTH", "131")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&status).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("15k/200k") && rendered.contains("(8%)"),
        "unexpected output: {rendered:?}"
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(&output.stdout)),
        "58caae96e138b15469aad1a7cc310bfeb7213731cf1e77933edd992bd601d5b1"
    );
}

#[test]
fn early_context_uses_223_environment_window_without_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("settings.json");
    fs::write(&config, include_bytes!("fixtures/settings.json")).unwrap();
    let reference = executable_script(temp.path(), "exit 97");
    let mut status: Value = serde_json::from_slice(include_bytes!("fixtures/status.json")).unwrap();
    status["transcript_path"] = Value::String(
        temp.path()
            .join("missing.jsonl")
            .to_string_lossy()
            .into_owned(),
    );
    status["context_window"]["context_window_size"] = Value::Null;
    status["context_window"]["current_usage"] = Value::Null;
    status["context_window"]["used_percentage"] = Value::Null;
    status["context_window"]["remaining_percentage"] = Value::Null;
    status["model"] = serde_json::json!({ "id": "claude-opus-4-6", "display_name": "Opus 4.6" });

    let mut child = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_NATIVE_FALLBACK", reference)
        .env("CCSTATUSLINE_CONTEXT_SIZE_FALLBACK", "333333px")
        .env("CCSTATUSLINE_WIDTH", "131")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&status).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("0/333k") && rendered.contains("(0%)"));
    assert_eq!(
        format!("{:x}", Sha256::digest(&output.stdout)),
        "a201760d4d81a955e737b4b3faeda14c6027254bfc9a90be0598b8bd0230f248"
    );
}

#[test]
fn exported_columns_supports_claude_code_captured_stdio() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("settings.json");
    fs::write(&config, include_bytes!("fixtures/settings.json")).unwrap();
    let input = include_bytes!("fixtures/status.json");

    let mut expected_child = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_WIDTH", "131")
        .env_remove("COLUMNS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    expected_child
        .stdin
        .take()
        .unwrap()
        .write_all(input)
        .unwrap();
    let expected = expected_child.wait_with_output().unwrap();
    assert!(expected.status.success());
    assert!(expected.stderr.is_empty());

    let mut child = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env_remove("CCSTATUSLINE_WIDTH")
        .env("COLUMNS", "131")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, expected.stdout);
}

#[test]
fn piped_stdin_uses_unexported_terminal_width_from_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("settings.json");
    fs::write(&config, include_bytes!("fixtures/settings.json")).unwrap();
    let reference = executable_script(temp.path(), "printf fallback");
    let input = include_bytes!("fixtures/status.json");

    let mut expected_child = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_WIDTH", "131")
        .env_remove("COLUMNS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    expected_child
        .stdin
        .take()
        .unwrap()
        .write_all(input)
        .unwrap();
    let expected = expected_child.wait_with_output().unwrap();
    assert!(expected.status.success());
    assert!(expected.stderr.is_empty());

    let (master, slave) = pseudo_terminal(131);
    let slave_guard = slave.try_clone().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_NATIVE_FALLBACK", reference)
        .env_remove("CCSTATUSLINE_WIDTH")
        .env_remove("COLUMNS")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    let actual = read_pty(master);
    drop(slave_guard);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(actual, expected.stdout);
}

#[test]
fn all_piped_stdio_uses_ancestor_terminal_width() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("settings.json");
    let status = temp.path().join("status.json");
    let actual_stdout = temp.path().join("stdout.bin");
    let actual_stderr = temp.path().join("stderr.bin");
    fs::write(&config, include_bytes!("fixtures/settings.json")).unwrap();
    fs::write(&status, include_bytes!("fixtures/status.json")).unwrap();

    let expected = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_WIDTH", "137")
        .env_remove("COLUMNS")
        .stdin(fs::File::open(&status).unwrap())
        .output()
        .unwrap();
    assert!(expected.status.success());
    assert!(expected.stderr.is_empty());

    let (master, slave) = pseudo_terminal(137);
    let helper = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ancestor_terminal_helper", "--nocapture"])
        .env("CCSTATUSLINE_NATIVE_ANCESTOR_HELPER", "1")
        .env("CCSTATUSLINE_NATIVE_TEST_CONFIG", &config)
        .env("CCSTATUSLINE_NATIVE_TEST_STATUS", &status)
        .env("CCSTATUSLINE_NATIVE_TEST_STDOUT", &actual_stdout)
        .env("CCSTATUSLINE_NATIVE_TEST_STDERR", &actual_stderr)
        .stdin(Stdio::from(slave))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    drop(master);

    assert!(
        helper.status.success(),
        "ancestor helper failed: {}",
        String::from_utf8_lossy(&helper.stderr)
    );
    assert_eq!(fs::read(actual_stdout).unwrap(), expected.stdout);
    assert!(fs::read(actual_stderr).unwrap().is_empty());
}

#[test]
fn ancestor_terminal_helper() {
    if std::env::var_os("CCSTATUSLINE_NATIVE_ANCESTOR_HELPER").is_none() {
        return;
    }

    // SAFETY: this dedicated helper process is not a process-group leader and
    // setsid has no memory-safety preconditions.
    assert_ne!(unsafe { libc::setsid() }, -1, "setsid failed");
    // SAFETY: stdin is the PTY slave supplied by the parent test. This helper
    // is now a session leader without a controlling terminal.
    #[cfg(target_os = "linux")]
    let set_controlling_terminal = libc::TIOCSCTTY;
    #[cfg(not(target_os = "linux"))]
    let set_controlling_terminal = libc::TIOCSCTTY.into();
    assert_ne!(
        unsafe { libc::ioctl(libc::STDIN_FILENO, set_controlling_terminal, 0) },
        -1,
        "TIOCSCTTY failed"
    );

    let config = std::env::var_os("CCSTATUSLINE_NATIVE_TEST_CONFIG").unwrap();
    let status = std::env::var_os("CCSTATUSLINE_NATIVE_TEST_STATUS").unwrap();
    let stdout_path = std::env::var_os("CCSTATUSLINE_NATIVE_TEST_STDOUT").unwrap();
    let stderr_path = std::env::var_os("CCSTATUSLINE_NATIVE_TEST_STDERR").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"));
    command
        .arg("--config")
        .arg(config)
        .env_remove("CCSTATUSLINE_WIDTH")
        .env_remove("COLUMNS")
        .stdin(fs::File::open(status).unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe and detaches only the native child,
    // leaving this helper as its TTY-owning ancestor.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let output = command.output().unwrap();
    assert!(output.status.success());
    fs::write(stdout_path, output.stdout).unwrap();
    fs::write(stderr_path, output.stderr).unwrap();
}

#[test]
fn failed_fallback_discards_partial_stdout_and_propagates_status() {
    let temp = tempfile::tempdir().unwrap();
    let config = unsupported_config(temp.path());
    let reference = executable_script(temp.path(), "printf partial; exit 7");
    let output = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_NATIVE_FALLBACK", reference)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("discarded 7 bytes")
    );
}

#[test]
fn check_config_json_contains_copyable_unsupported_paths() {
    let temp = tempfile::tempdir().unwrap();
    let config = unsupported_config(temp.path());
    let output = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args([
            "--check-config",
            "--format",
            "json",
            "--config",
            config.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["supported"], false);
    assert_eq!(report["issues"][0]["path"], "/lines/0/0/type");
    assert_eq!(report["issues"][0]["value"], "session-cost");
}
