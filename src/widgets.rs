use crate::config::WidgetItem;
use crate::git::GitResolver;
use crate::render::RenderError;
use crate::status::StatusInput;
use regex::Regex;
use std::env;
use std::path::Path;
use std::sync::LazyLock;

static MODEL_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*\(.*\)$").expect("valid model suffix regex"));

pub fn render(
    item: &WidgetItem,
    status: &StatusInput,
    git: &mut GitResolver,
) -> Result<Option<String>, RenderError> {
    match item.kind.as_str() {
        "vim-mode" => render_vim_mode(item, status),
        "context-bar" => render_context_bar(item, status),
        "model" => Ok(render_model(item, status)),
        "thinking-effort" => Ok(Some(render_thinking_effort(item, status))),
        "current-working-dir" => Ok(render_cwd(item, status)),
        "git-branch" => Ok(Some(render_git_branch(item, status, git))),
        "custom-command"
            if item.command_path.as_deref() == Some(crate::git::GIT_SUMMARY_COMMAND) =>
        {
            Ok(Some(render_git_summary(status, git)))
        }
        kind => Err(RenderError::NeedsFallback(format!(
            "widget `{kind}` passed capability validation unexpectedly"
        ))),
    }
}

fn render_vim_mode(item: &WidgetItem, status: &StatusInput) -> Result<Option<String>, RenderError> {
    let Some(mode) = status.vim_mode() else {
        return Ok(None);
    };
    let format = item
        .metadata
        .get("format")
        .map(String::as_str)
        .unwrap_or("icon-dash-letter");
    let icon = if item.metadata.get("nerdFont").map(String::as_str) == Some("true") {
        "\u{e62b}"
    } else {
        "v"
    };
    if matches!(format, "letter" | "icon-letter" | "icon-dash-letter")
        && mode
            .chars()
            .next()
            .is_some_and(|character| character as u32 > 0xffff)
    {
        return Err(RenderError::NeedsFallback(
            "Vim letter format begins with a non-BMP UTF-16 character".into(),
        ));
    }
    let letter = match mode {
        "NORMAL" => "N".to_string(),
        "INSERT" => "I".to_string(),
        _ => match mode.chars().next() {
            None => String::new(),
            // JavaScript mode[0] returns a lone UTF-16 surrogate here; Bun's
            // stdout encoding replaces it with U+FFFD.
            Some(character) if character as u32 > 0xffff => "�".to_string(),
            Some(character) => character.to_string(),
        },
    };
    Ok(Some(match format {
        "icon-dash-letter" => format!("{icon}-{letter}"),
        "icon-letter" => format!("{icon} {letter}"),
        "icon" => icon.to_string(),
        "letter" => letter,
        "word" => mode.to_string(),
        _ => unreachable!("config validator rejects unknown Vim formats"),
    }))
}

fn render_context_bar(
    item: &WidgetItem,
    status: &StatusInput,
) -> Result<Option<String>, RenderError> {
    let metrics = status.context_window_metrics();
    let transcript_path = status
        .transcript_path()
        .filter(|path| !path.is_empty())
        .map(Path::new);
    let used = match (metrics.context_length_tokens, transcript_path) {
        (Some(used), _) => Some(used),
        (None, Some(path)) => Some(
            crate::context::transcript_context_length(path)
                .map_err(|error| RenderError::NeedsFallback(error.to_string()))?,
        ),
        (None, None) => None,
    };
    let total = metrics
        .window_size
        .or_else(|| transcript_path.map(|_| crate::context::model_context_window_size(status)));
    let (Some(used), Some(total)) = (used, total) else {
        return Ok(None);
    };
    if total <= 0.0 {
        return Ok(None);
    }
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    if used.fract() != 0.0
        || total.fract() != 0.0
        || used > MAX_SAFE_INTEGER
        || total > MAX_SAFE_INTEGER
    {
        return Err(RenderError::NeedsFallback(
            "context-bar has fractional or unsafe-integer token metrics".into(),
        ));
    }

    let percent = (used / total * 100.0).clamp(0.0, 100.0);
    let width = if item.metadata.get("display").map(String::as_str) == Some("progress") {
        32
    } else {
        16
    };
    let filled = js_round(percent / 100.0 * width as f64).clamp(0.0, width as f64) as usize;
    let display = format!(
        "[{}{}] {}/{} ({}%)",
        "█".repeat(filled),
        "░".repeat(width - filled),
        format_tokens(used, 0),
        format_tokens(total, 0),
        js_round(percent) as i64
    );
    Ok(Some(if item.raw_value.unwrap_or(false) {
        display
    } else {
        format!("Context: {display}")
    }))
}

fn render_model(item: &WidgetItem, status: &StatusInput) -> Option<String> {
    let model = status.model_display_name()?;
    if model.is_empty() {
        return None;
    }
    let short = remove_parenthesized_suffix(model);
    Some(if item.raw_value.unwrap_or(false) {
        short.to_string()
    } else {
        format!("Model: {short}")
    })
}

fn render_thinking_effort(item: &WidgetItem, status: &StatusInput) -> String {
    let effort = crate::effort::resolve(status).display().into_owned();
    if item.raw_value.unwrap_or(false) {
        effort
    } else {
        format!("Thinking: {effort}")
    }
}

fn render_cwd(item: &WidgetItem, status: &StatusInput) -> Option<String> {
    let cwd = status.cwd()?;
    if cwd.is_empty() {
        return None;
    }
    Some(if item.raw_value.unwrap_or(false) {
        cwd.to_string()
    } else {
        format!("cwd: {cwd}")
    })
}

fn render_git_branch(item: &WidgetItem, status: &StatusInput, git: &mut GitResolver) -> String {
    let cwd = git_cwd(status);
    match git.branch(&cwd) {
        Some(branch) if item.raw_value.unwrap_or(false) => branch,
        Some(branch) => format!("⎇ {branch}"),
        None => "⎇ no git".to_string(),
    }
}

/// Render the one custom command deliberately implemented as a native
/// intrinsic. `--git-summary` reuses this function so fallback execution and
/// direct native rendering cannot drift apart.
pub(crate) fn render_git_summary(status: &StatusInput, git: &mut GitResolver) -> String {
    match git.summary(&git_cwd(status)) {
        Ok(Some(snapshot)) => snapshot.compact(),
        Ok(None) => "⎇ no git".to_string(),
        Err(error) if error.is_timeout() => "[Timeout]".to_string(),
        Err(_) => "[Exit: 1]".to_string(),
    }
}

fn git_cwd(status: &StatusInput) -> std::path::PathBuf {
    status
        .git_cwd()
        .map(Path::new)
        .map(ToOwned::to_owned)
        .or_else(|| env::current_dir().ok())
        .unwrap_or_default()
}

fn remove_parenthesized_suffix(model: &str) -> &str {
    MODEL_SUFFIX
        .find(model)
        .map_or(model, |suffix| &model[..suffix.start()])
}

fn format_tokens(count: f64, decimals: usize) -> String {
    let million_boundary = 1_000_000.0 - 500.0 / 10_f64.powi(decimals as i32);
    if count >= million_boundary {
        return format!("{}M", javascript_to_fixed(count / 1_000_000.0, 1));
    }
    if count >= 1_000.0 {
        return format!("{}k", javascript_to_fixed(count / 1_000.0, decimals));
    }
    (count as u64).to_string()
}

/// ECMAScript `Number.prototype.toFixed` rounds the exact binary float, with
/// ties toward the larger integer. Rust formatting uses ties-to-even instead,
/// so values such as 2.5 need this small exact-rational conversion.
fn javascript_to_fixed(value: f64, decimals: usize) -> String {
    debug_assert!(value.is_finite() && value >= 0.0);
    let decimal_scale = 10_u128.pow(decimals as u32);
    let bits = value.to_bits();
    let stored_exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    let (significand, binary_exponent) = if stored_exponent == 0 {
        (fraction, -1022 - 52)
    } else {
        (fraction | (1_u64 << 52), stored_exponent - 1023 - 52)
    };
    let numerator = u128::from(significand) * decimal_scale;
    let rounded = if binary_exponent >= 0 {
        numerator << binary_exponent as u32
    } else {
        let shift = (-binary_exponent) as u32;
        if shift >= 128 {
            0
        } else {
            let denominator = 1_u128 << shift;
            let quotient = numerator / denominator;
            let remainder = numerator % denominator;
            quotient + u128::from(remainder * 2 >= denominator)
        }
    };

    if decimals == 0 {
        return rounded.to_string();
    }
    let whole = rounded / decimal_scale;
    let fraction = rounded % decimal_scale;
    format!("{whole}.{fraction:0decimals$}")
}

fn js_round(value: f64) -> f64 {
    (value + 0.5).floor()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn item(kind: &str) -> WidgetItem {
        serde_json::from_value(json!({ "id": "test", "type": kind })).unwrap()
    }

    fn status(value: serde_json::Value) -> StatusInput {
        StatusInput::from_value(value).unwrap()
    }

    #[test]
    fn model_suffix_removal_is_greedy_like_the_reference_regex() {
        assert_eq!(remove_parenthesized_suffix("Opus (first) (second)"), "Opus");
        assert_eq!(remove_parenthesized_suffix("Opus(foo)\n(bar)"), "Opus(foo)");
        assert_eq!(remove_parenthesized_suffix("Opus"), "Opus");
    }

    #[test]
    fn vim_formats_match_javascript_string_indexing_edges() {
        let mut vim = item("vim-mode");
        let empty = status(json!({ "vim": { "mode": "" } }));

        vim.metadata.insert("format".into(), "icon".into());
        assert_eq!(render_vim_mode(&vim, &empty).unwrap().as_deref(), Some("v"));
        vim.metadata
            .insert("format".into(), "icon-dash-letter".into());
        assert_eq!(
            render_vim_mode(&vim, &empty).unwrap().as_deref(),
            Some("v-")
        );
        vim.metadata.insert("format".into(), "icon-letter".into());
        assert_eq!(
            render_vim_mode(&vim, &empty).unwrap().as_deref(),
            Some("v ")
        );

        let emoji = status(json!({ "vim": { "mode": "😀" } }));
        vim.metadata.insert("format".into(), "letter".into());
        assert!(matches!(
            render_vim_mode(&vim, &emoji),
            Err(RenderError::NeedsFallback(_))
        ));
    }

    #[test]
    fn context_excludes_output_and_uses_medium_bar() {
        let output = render_context_bar(
            &item("context-bar"),
            &status(json!({
                "context_window": {
                    "context_window_size": 200000,
                    "current_usage": {
                        "input_tokens": 10000,
                        "output_tokens": 99999,
                        "cache_creation_input_tokens": 10000,
                        "cache_read_input_tokens": 10000
                    }
                }
            })),
        )
        .unwrap()
        .unwrap();
        assert_eq!(output, "Context: [██░░░░░░░░░░░░░░] 30k/200k (15%)");
    }

    #[test]
    fn context_uses_transcript_when_live_usage_is_null() {
        let mut transcript = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            transcript,
            "{}",
            json!({
                "timestamp": "2026-07-11T01:02:03.000Z",
                "message": {
                    "stop_reason": "end_turn",
                    "usage": {
                        "input_tokens": 10000,
                        "output_tokens": 99999,
                        "cache_creation_input_tokens": 5000,
                        "cache_read_input_tokens": 15000
                    }
                }
            })
        )
        .unwrap();
        let output = render_context_bar(
            &item("context-bar"),
            &status(json!({
                "transcript_path": transcript.path(),
                "context_window": {
                    "context_window_size": 200000,
                    "current_usage": null,
                    "used_percentage": null,
                    "remaining_percentage": null
                }
            })),
        )
        .unwrap()
        .unwrap();

        assert_eq!(output, "Context: [██░░░░░░░░░░░░░░] 30k/200k (15%)");
    }

    #[test]
    fn transcript_metrics_enable_model_window_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let output = render_context_bar(
            &item("context-bar"),
            &status(json!({
                "transcript_path": directory.path().join("missing.jsonl"),
                "model": { "id": "claude-opus", "display_name": "Opus (1M)" }
            })),
        )
        .unwrap()
        .unwrap();
        assert_eq!(output, "Context: [░░░░░░░░░░░░░░░░] 0/1.0M (0%)");

        assert_eq!(
            render_context_bar(&item("context-bar"), &status(json!({}))).unwrap(),
            None
        );
        assert_eq!(
            render_context_bar(
                &item("context-bar"),
                &status(json!({ "transcript_path": "" }))
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn live_context_metrics_do_not_read_the_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let incompatible = directory.path().join("incompatible.jsonl");
        std::fs::write(
            &incompatible,
            r#"{"timestamp":"not canonical","message":{"usage":{"input_tokens":999}}}"#,
        )
        .unwrap();

        let output = render_context_bar(
            &item("context-bar"),
            &status(json!({
                "transcript_path": incompatible,
                "context_window": {
                    "context_window_size": 200000,
                    "current_usage": { "input_tokens": 10000 }
                }
            })),
        )
        .unwrap()
        .unwrap();
        assert!(output.contains("10k/200k (5%)"));
    }

    #[test]
    fn explicit_empty_model_is_hidden() {
        let model = item("model");
        assert_eq!(
            render_model(
                &model,
                &status(json!({ "model": { "display_name": "", "id": "opus" } }))
            ),
            None
        );
    }

    #[test]
    fn token_rounding_matches_javascript_ties() {
        assert_eq!(format_tokens(2_500.0, 0), "3k");
        assert_eq!(format_tokens(3_500.0, 0), "4k");
        assert_eq!(format_tokens(1_250_000.0, 0), "1.3M");
        assert_eq!(format_tokens(2_150_000.0, 0), "2.1M");
        assert_eq!(format_tokens(2_550_000.0, 0), "2.5M");
    }

    #[test]
    fn intrinsic_git_summary_handles_a_non_repository_in_process() {
        let directory = tempfile::tempdir().unwrap();
        let status = status(json!({ "cwd": directory.path() }));
        let mut git = GitResolver::new(0.0);
        assert_eq!(render_git_summary(&status, &mut git), "⎇ no git");
    }
}
