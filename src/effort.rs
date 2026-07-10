//! Thinking-effort resolution compatible with ccstatusline 2.2.22.

use crate::status::{StatusEffort, StatusInput};
use regex::Regex;
use serde_json::Value;
use std::borrow::Cow;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

static ANSI_SEQUENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))").expect("valid ANSI regex")
});
static MODEL_EFFORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)^<local-command-stdout>Set model to.*? with ([a-zA-Z0-9-]+) effort</local-command-stdout>$",
    )
    .expect("valid model effort regex")
});
static COMMAND_EFFORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^<local-command-stdout>Set effort level to ([a-zA-Z0-9-]+)\b")
        .expect("valid command effort regex")
});
static CUSTOM_EFFORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9-]{2,20}$").expect("valid custom effort regex"));

const MODEL_PREFIX: &str = "<local-command-stdout>Set model to ";
const COMMAND_PREFIX: &str = "<local-command-stdout>Set effort level to ";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEffort {
    pub value: String,
    pub known: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveEffort {
    Default,
    Level(ResolvedEffort),
}

impl EffectiveEffort {
    pub fn display(&self) -> Cow<'_, str> {
        match self {
            Self::Default => Cow::Borrowed("default"),
            Self::Level(level) if level.known => Cow::Borrowed(&level.value),
            Self::Level(level) => Cow::Owned(format!("{}?", level.value)),
        }
    }
}

pub fn normalize(value: &str) -> Option<ResolvedEffort> {
    if value.is_empty() {
        return None;
    }

    let normalized = value.to_lowercase();
    let known = matches!(
        normalized.as_str(),
        "low" | "medium" | "high" | "xhigh" | "max"
    );
    let valid_custom = CUSTOM_EFFORT.is_match(&normalized)
        && normalized.bytes().any(|byte| byte.is_ascii_alphanumeric());
    if known || valid_custom {
        Some(ResolvedEffort {
            value: normalized,
            known,
        })
    } else {
        None
    }
}

/// Resolve live status, transcript, Claude settings, then the default.
pub fn resolve(status: &StatusInput) -> EffectiveEffort {
    let transcript_path = status.transcript_path().map(Path::new);
    let settings_path = default_settings_path();
    resolve_from_paths(status, transcript_path, settings_path.as_deref())
}

/// Injectable form used by tests and callers that already know the paths.
pub fn resolve_from_paths(
    status: &StatusInput,
    transcript_path: Option<&Path>,
    settings_path: Option<&Path>,
) -> EffectiveEffort {
    match status.effort() {
        StatusEffort::ExplicitDefault => return EffectiveEffort::Default,
        StatusEffort::Level(level) => {
            if let Some(level) = normalize(level) {
                return EffectiveEffort::Level(level);
            }
        }
        StatusEffort::Missing => {}
    }

    if let Some(level) = transcript_path.and_then(transcript_effort) {
        return EffectiveEffort::Level(level);
    }
    if let Some(level) = settings_path.and_then(settings_effort) {
        return EffectiveEffort::Level(level);
    }
    EffectiveEffort::Default
}

pub fn default_settings_path() -> Option<PathBuf> {
    if let Some(config_dir) = env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty()) {
        let config_dir = PathBuf::from(config_dir);
        let resolved = if config_dir.is_absolute() {
            config_dir
        } else {
            env::current_dir().ok()?.join(config_dir)
        };

        match fs::metadata(&resolved) {
            Ok(metadata) if metadata.is_dir() => return Some(resolved.join("settings.json")),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Some(resolved.join("settings.json"));
            }
            Err(_) => {}
        }
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude/settings.json"))
}

pub fn settings_effort(path: &Path) -> Option<ResolvedEffort> {
    let bytes = fs::read(path).ok()?;
    let settings: Value = serde_json::from_slice(&bytes).ok()?;
    normalize(settings.get("effortLevel")?.as_str()?)
}

/// Scan newest-to-oldest. Encountering a `/model` row without an effort is a
/// barrier: an older selection is stale, so settings must supply the fallback.
pub fn transcript_effort(path: &Path) -> Option<ResolvedEffort> {
    let transcript = fs::read_to_string(path).ok()?;

    for line in transcript.lines().rev() {
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(content) = entry
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
        else {
            continue;
        };

        let visible = ANSI_SEQUENCE.replace_all(content, "");
        let visible = visible.trim();

        if visible.starts_with(COMMAND_PREFIX) {
            if let Some(capture) = COMMAND_EFFORT.captures(visible) {
                return normalize(&capture[1]);
            }
        }

        if !visible.starts_with(MODEL_PREFIX) {
            continue;
        }

        return MODEL_EFFORT
            .captures(visible)
            .and_then(|capture| normalize(&capture[1]));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn status(value: Value) -> StatusInput {
        StatusInput::from_value(value).unwrap()
    }

    fn transcript(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for content in lines {
            writeln!(
                file,
                "{}",
                json!({ "type": "user", "message": { "content": content } })
            )
            .unwrap();
        }
        file
    }

    #[test]
    fn normalizes_known_and_safe_custom_levels() {
        assert_eq!(normalize("xHigh").unwrap().value, "xhigh");
        assert!(normalize("xHigh").unwrap().known);
        assert_eq!(
            normalize("Super-Max"),
            Some(ResolvedEffort {
                value: "super-max".into(),
                known: false,
            })
        );
        assert_eq!(normalize("x"), None);
        assert_eq!(normalize("has space"), None);
    }

    #[test]
    fn latest_relevant_transcript_row_wins() {
        let transcript = transcript(&[
            "<local-command-stdout>Set effort level to \u{1b}[1mmax\u{1b}[22m (this session only)</local-command-stdout>",
            "<local-command-stdout>Set model to sonnet with low effort</local-command-stdout>",
        ]);
        assert_eq!(transcript_effort(transcript.path()).unwrap().value, "low");
    }

    #[test]
    fn newer_model_without_effort_invalidates_stale_transcript_effort() {
        let transcript = transcript(&[
            "<local-command-stdout>Set effort level to high</local-command-stdout>",
            "<local-command-stdout>Set model to sonnet</local-command-stdout>",
        ]);
        assert_eq!(transcript_effort(transcript.path()), None);
    }

    #[test]
    fn explicit_default_stops_all_fallbacks() {
        let transcript =
            transcript(&["<local-command-stdout>Set effort level to high</local-command-stdout>"]);
        let resolved = resolve_from_paths(
            &status(json!({ "effort": { "level": null } })),
            Some(transcript.path()),
            None,
        );
        assert_eq!(resolved, EffectiveEffort::Default);
    }

    #[test]
    fn settings_are_the_last_nondefault_source() {
        let mut settings = tempfile::NamedTempFile::new().unwrap();
        write!(settings, "{}", json!({ "effortLevel": "Ultra" })).unwrap();
        let resolved = resolve_from_paths(&status(json!({})), None, Some(settings.path()));
        assert_eq!(resolved.display(), "ultra?");
    }
}
