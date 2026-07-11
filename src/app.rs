use crate::config::{self, SupportReport};
use crate::render;
use crate::status::StatusInput;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{ExitCode, ExitStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Render,
    GitSummary,
    Check,
    SupportReport,
    Help,
    Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug)]
struct Cli {
    action: Action,
    format: OutputFormat,
    config: PathBuf,
}

pub fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let cli = match parse_cli(args) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("{}: {message}\n\nRun with --help for usage.", crate::NAME);
            return ExitCode::from(2);
        }
    };

    match cli.action {
        Action::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        Action::Version => {
            println!(
                "{} {} (compatible with ccstatusline {})",
                crate::NAME,
                env!("CARGO_PKG_VERSION"),
                crate::REFERENCE_CCSTATUSLINE_VERSION
            );
            ExitCode::SUCCESS
        }
        Action::Check | Action::SupportReport => inspect_config(&cli),
        Action::GitSummary => run_git_summary(),
        Action::Render if io::stdin().is_terminal() => run_tui(&cli.config),
        Action::Render => run_renderer(&cli.config),
    }
}

fn parse_cli(args: impl Iterator<Item = OsString>) -> Result<Cli, String> {
    let mut action = Action::Render;
    let mut format = OutputFormat::Human;
    let mut config = config::default_config_path();
    let mut args = args.peekable();
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--config") => {
                config = PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--config requires a path".to_string())?,
                );
            }
            Some("--check-config") => set_action(&mut action, Action::Check)?,
            Some("--support-report") => set_action(&mut action, Action::SupportReport)?,
            Some("--git-summary") => set_action(&mut action, Action::GitSummary)?,
            Some("--format") => {
                format = match args.next().as_deref().and_then(|value| value.to_str()) {
                    Some("human") => OutputFormat::Human,
                    Some("json") => OutputFormat::Json,
                    Some(value) => return Err(format!("unknown output format `{value}`")),
                    None => return Err("--format requires `human` or `json`".into()),
                };
            }
            Some("-h" | "--help") => set_action(&mut action, Action::Help)?,
            Some("-V" | "--version") => set_action(&mut action, Action::Version)?,
            Some(value) => return Err(format!("unknown argument `{value}`")),
            None => return Err("arguments must be valid UTF-8".into()),
        }
    }
    Ok(Cli {
        action,
        format,
        config,
    })
}

fn set_action(current: &mut Action, next: Action) -> Result<(), String> {
    if *current != Action::Render && *current != next {
        return Err(
            "choose only one of --git-summary, --check-config, --support-report, --help, or --version"
                .into(),
        );
    }
    *current = next;
    Ok(())
}

fn run_git_summary() -> ExitCode {
    let mut stdin = Vec::new();
    if let Err(error) = io::stdin().read_to_end(&mut stdin) {
        eprintln!("{}: cannot read status JSON: {error}", crate::NAME);
        return ExitCode::FAILURE;
    }
    let status = match StatusInput::parse(&stdin) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("{}: {error}", crate::NAME);
            return ExitCode::from(2);
        }
    };

    // The helper is deliberately configuration-independent. When the
    // reference renderer invokes it as a custom command, this path cannot
    // recurse through config validation or the JavaScript fallback.
    let mut git = crate::git::GitResolver::new(5.0);
    let output = crate::widgets::render_git_summary(&status, &mut git);
    if let Err(error) = writeln!(io::stdout(), "{output}") {
        eprintln!("{}: cannot write Git summary: {error}", crate::NAME);
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn inspect_config(cli: &Cli) -> ExitCode {
    let loaded = match config::load(&cli.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{}: {error}", crate::NAME);
            return ExitCode::from(2);
        }
    };
    let report = config::support_report(&loaded);
    match cli.format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).expect("support report is serializable")
            );
        }
        OutputFormat::Human if cli.action == Action::SupportReport => {
            println!("{}", report.markdown());
        }
        OutputFormat::Human if report.supported => {
            println!(
                "Fast path enabled: {} is supported by {}.",
                loaded.path.display(),
                crate::NAME
            );
        }
        OutputFormat::Human => print_unsupported_report(&report),
    }
    if report.supported || cli.action == Action::SupportReport {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn run_renderer(config_path: &std::path::Path) -> ExitCode {
    let mut stdin = Vec::new();
    if let Err(error) = io::stdin().read_to_end(&mut stdin) {
        eprintln!("{}: cannot read status JSON: {error}", crate::NAME);
        return ExitCode::FAILURE;
    }

    let loaded = match config::load(config_path) {
        Ok(config) => config,
        Err(error) => {
            warn_fallback(&error.to_string(), None);
            return delegate_render(config_path, &stdin);
        }
    };
    let report = config::support_report(&loaded);
    if !report.supported {
        warn_fallback(&report.summary(), Some(&report));
        return delegate_render(config_path, &stdin);
    }

    let status = match StatusInput::parse(&stdin) {
        Ok(status) => status,
        Err(error) => {
            warn_fallback(&error.to_string(), None);
            return delegate_render(config_path, &stdin);
        }
    };
    let width = crate::terminal::width();
    match render::render(&loaded.settings, &status, width) {
        Ok(output) => {
            if let Err(error) = io::stdout().write_all(output.as_bytes()) {
                eprintln!(
                    "{}: cannot write rendered status line: {error}",
                    crate::NAME
                );
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            warn_fallback(&error.to_string(), None);
            delegate_render(config_path, &stdin)
        }
    }
}

fn run_tui(config_path: &std::path::Path) -> ExitCode {
    let status = match crate::fallback::tui(config_path) {
        Ok(status) => status,
        Err(error) => {
            eprintln!("{}: {error}", crate::NAME);
            return ExitCode::FAILURE;
        }
    };
    if !status.success() {
        return exit_code(status);
    }

    match config::load(config_path) {
        Ok(config) => {
            let report = config::support_report(&config);
            if !report.supported {
                eprintln!();
                print_unsupported_report_stderr(&report);
            }
        }
        Err(error) => eprintln!(
            "\n{}: TUI exited successfully, but the resulting config cannot be checked: {error}",
            crate::NAME
        ),
    }
    ExitCode::SUCCESS
}

fn delegate_render(config_path: &std::path::Path, stdin: &[u8]) -> ExitCode {
    match crate::fallback::render(config_path, stdin) {
        Ok(output) if output.status.success() => {
            if let Err(error) = io::stdout().write_all(&output.stdout) {
                eprintln!("{}: cannot write fallback output: {error}", crate::NAME);
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(output) => {
            eprintln!(
                "{}: ccstatusline fallback failed; discarded {} bytes of partial stdout",
                crate::NAME,
                output.stdout.len()
            );
            exit_code(output.status)
        }
        Err(error) => {
            eprintln!("{}: {error}", crate::NAME);
            ExitCode::FAILURE
        }
    }
}

fn warn_fallback(reason: &str, report: Option<&SupportReport>) {
    eprintln!(
        "{}: fast path disabled; using ccstatusline {}: {reason}",
        crate::NAME,
        crate::REFERENCE_CCSTATUSLINE_VERSION
    );
    if let Some(report) = report {
        eprintln!(
            "{}: run `{} --support-report --config {}` for a copyable implementation request",
            crate::NAME,
            crate::NAME,
            report.config_path.display()
        );
    }
}

fn print_unsupported_report(report: &SupportReport) {
    println!(
        "Fast path disabled; ccstatusline {} will be used.\n\nCopy this implementation request:\n\n{}",
        crate::REFERENCE_CCSTATUSLINE_VERSION,
        report.markdown()
    );
}

fn print_unsupported_report_stderr(report: &SupportReport) {
    eprintln!(
        "Fast path disabled; ccstatusline {} will be used.\n\nCopy this implementation request:\n\n{}",
        crate::REFERENCE_CCSTATUSLINE_VERSION,
        report.markdown()
    );
}

fn exit_code(status: ExitStatus) -> ExitCode {
    ExitCode::from(status.code().unwrap_or(1).clamp(1, 255) as u8)
}

fn print_help() {
    println!(
        "{name} — fast native ccstatusline renderer\n\n\
Usage:\n  {name} [--config PATH]\n  {name} --check-config [--format human|json] [--config PATH]\n  {name} --support-report [--format human|json] [--config PATH]\n  {name} --git-summary < status.json\n\n\
With piped Claude Code status JSON, render the status line. With a terminal on\n\
stdin, open ccstatusline {reference}'s TUI and check the saved config afterward.\n\n\
Options:\n  --config PATH       Override ~/.config/ccstatusline/settings.json\n  --check-config      Exit 0 when the native fast path supports the config\n  --support-report    Print a copyable compatibility request\n  --git-summary       Print the intrinsic rich Git summary without loading config\n  --format FORMAT     human (default) or json\n  -V, --version       Print version information\n  -h, --help          Print this help",
        name = crate::NAME,
        reference = crate::REFERENCE_CCSTATUSLINE_VERSION,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, String> {
        parse_cli(args.iter().map(OsString::from))
    }

    #[test]
    fn parses_standalone_git_summary_action() {
        let cli = parse(&["--git-summary"]).unwrap();
        assert_eq!(cli.action, Action::GitSummary);
    }

    #[test]
    fn git_summary_conflicts_with_other_actions() {
        let error = parse(&["--git-summary", "--check-config"]).unwrap_err();
        assert!(error.contains("--git-summary"));
        assert!(error.contains("--check-config"));
    }
}
