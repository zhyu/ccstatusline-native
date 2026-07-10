#![cfg(unix)]

use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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
fn missing_width_delegates_instead_of_guessing_flex_layout() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("settings.json");
    fs::write(&config, include_bytes!("fixtures/settings.json")).unwrap();
    let reference = executable_script(temp.path(), "cat");
    let input = include_bytes!("fixtures/status.json");

    let mut child = Command::new(env!("CARGO_BIN_EXE_ccstatusline-native"))
        .args(["--config", config.to_str().unwrap()])
        .env("CCSTATUSLINE_NATIVE_FALLBACK", reference)
        .env_remove("CCSTATUSLINE_WIDTH")
        .env_remove("COLUMNS")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert_eq!(output.stdout, input);
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("terminal width is unavailable")
    );
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
