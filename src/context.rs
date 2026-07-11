//! Context-window fallbacks compatible with ccstatusline's transcript metrics.

use crate::status::StatusInput;
use regex::Regex;
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

const DEFAULT_CONTEXT_WINDOW_SIZE: f64 = 200_000.0;
const CONTEXT_SIZE_FALLBACK_ENV_VAR: &str = "CCSTATUSLINE_CONTEXT_SIZE_FALLBACK";

static DELIMITED_CONTEXT_SIZE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:\(|\[)\s*(\d+(?:[,_]\d+)*(?:\.\d+)?)\s*([km])\s*(?:\)|\])")
        .expect("valid delimited context-size regex")
});
static LABELED_CONTEXT_SIZE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\d+(?:[,_]\d+)*(?:\.\d+)?)\s*([km])(?:\s*(?:token\s*)?context)?\b")
        .expect("valid labeled context-size regex")
});

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("transcript context metrics contain {0}")]
    Unsupported(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Timestamp {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    millisecond: u16,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    timestamp: Timestamp,
    context_length: f64,
}

/// Reproduce the context-length part of ccstatusline's transcript token metrics.
///
/// A path that cannot be read has zero metrics in the reference implementation.
/// Structurally incompatible, otherwise eligible rows are reported so the caller
/// can delegate rather than silently render a different value.
pub fn transcript_context_length(path: &Path) -> Result<f64, ContextError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(0.0),
    };
    let transcript = String::from_utf8_lossy(&bytes);

    let mut saw_stop_reason = false;
    let mut best_legacy = None;
    let mut best_finalized = None;
    let mut final_unfinished = None;

    for line in transcript.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(message) = row.get("message").and_then(Value::as_object) else {
            continue;
        };
        let Some(raw_usage) = message.get("usage") else {
            continue;
        };
        if !javascript_truthy(raw_usage) {
            continue;
        }
        let usage = raw_usage.as_object().ok_or(ContextError::Unsupported(
            "a non-object message.usage value",
        ))?;

        let stop_reason = message.get("stop_reason");
        saw_stop_reason |= message.contains_key("stop_reason");
        let candidate = candidate(&row, usage)?;
        if let Some(candidate) = candidate {
            choose_newer(&mut best_legacy, candidate);
            if stop_reason.is_some_and(javascript_truthy) {
                choose_newer(&mut best_finalized, candidate);
            }
        }

        // ccstatusline retains an unfinished row only when it is the final
        // parsed usage row, so every later usage row replaces this state.
        final_unfinished = if stop_reason == Some(&Value::Null) {
            candidate
        } else {
            None
        };
    }

    let selected = if saw_stop_reason {
        if let Some(candidate) = final_unfinished {
            choose_newer(&mut best_finalized, candidate);
        }
        best_finalized
    } else {
        best_legacy
    };
    Ok(selected.map_or(0.0, |candidate| candidate.context_length))
}

/// Infer the context-window size exactly where ContextBar uses token metrics.
pub fn model_context_window_size(status: &StatusInput) -> f64 {
    let fallback = std::env::var(CONTEXT_SIZE_FALLBACK_ENV_VAR).ok();
    model_context_window_size_with_fallback(status, fallback.as_deref())
}

fn model_context_window_size_with_fallback(status: &StatusInput, fallback: Option<&str>) -> f64 {
    status
        .model_context_identifier()
        .as_deref()
        .and_then(parse_context_window_size)
        .or_else(|| {
            fallback
                .and_then(crate::terminal::parse_positive_decimal_prefix)
                .map(|size| size as f64)
        })
        .unwrap_or(DEFAULT_CONTEXT_WINDOW_SIZE)
}

fn candidate(row: &Value, usage: &Map<String, Value>) -> Result<Option<Candidate>, ContextError> {
    if row.get("isSidechain") == Some(&Value::Bool(true))
        || row.get("isApiErrorMessage").is_some_and(javascript_truthy)
    {
        return Ok(None);
    }

    let Some(raw_timestamp) = row.get("timestamp") else {
        return Ok(None);
    };
    if !javascript_truthy(raw_timestamp) {
        return Ok(None);
    }
    let timestamp = raw_timestamp
        .as_str()
        .and_then(parse_canonical_timestamp)
        .ok_or(ContextError::Unsupported("a non-canonical timestamp"))?;
    let context_length = usage_number_or_zero(usage, "input_tokens", true)?
        + usage_number_or_zero(usage, "cache_read_input_tokens", false)?
        + usage_number_or_zero(usage, "cache_creation_input_tokens", false)?;

    Ok(Some(Candidate {
        timestamp,
        context_length,
    }))
}

fn usage_number_or_zero(
    usage: &Map<String, Value>,
    key: &'static str,
    javascript_or: bool,
) -> Result<f64, ContextError> {
    let Some(value) = usage.get(key) else {
        return Ok(0.0);
    };
    if value.is_null() || (javascript_or && !javascript_truthy(value)) {
        return Ok(0.0);
    }
    value
        .as_f64()
        .ok_or(ContextError::Unsupported("a non-numeric token count"))
}

fn choose_newer(best: &mut Option<Candidate>, candidate: Candidate) {
    if best.is_none_or(|current| candidate.timestamp > current.timestamp) {
        *best = Some(candidate);
    }
}

fn javascript_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Claude transcripts use `Date.toISOString()` timestamps. Supporting this
/// canonical UTC form keeps ordering dependency-free; other truthy timestamp
/// shapes delegate instead of guessing at JavaScript Date parsing.
fn parse_canonical_timestamp(value: &str) -> Option<Timestamp> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || *bytes.last()? != b'Z'
    {
        return None;
    }

    let year = digits(bytes, 0, 4)? as u16;
    let month = digits(bytes, 5, 2)? as u8;
    let day = digits(bytes, 8, 2)? as u8;
    let hour = digits(bytes, 11, 2)? as u8;
    let minute = digits(bytes, 14, 2)? as u8;
    let second = digits(bytes, 17, 2)? as u8;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let fraction = &bytes[19..bytes.len() - 1];
    let millisecond = match fraction {
        [] => 0,
        [b'.', digits @ ..] if !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) => {
            let mut milliseconds = 0_u16;
            for index in 0..3 {
                milliseconds *= 10;
                if let Some(digit) = digits.get(index) {
                    milliseconds += u16::from(*digit - b'0');
                }
            }
            milliseconds
        }
        _ => return None,
    };

    Some(Timestamp {
        year,
        month,
        day,
        hour,
        minute,
        second,
        millisecond,
    })
}

fn digits(bytes: &[u8], start: usize, length: usize) -> Option<u32> {
    bytes
        .get(start..start + length)?
        .iter()
        .try_fold(0_u32, |value, digit| {
            digit
                .is_ascii_digit()
                .then_some(value * 10 + u32::from(*digit - b'0'))
        })
}

fn parse_context_window_size(identifier: &str) -> Option<f64> {
    [&*DELIMITED_CONTEXT_SIZE, &*LABELED_CONTEXT_SIZE]
        .into_iter()
        .find_map(|pattern| pattern.captures(identifier))
        .and_then(|captures| {
            let number = captures.get(1)?.as_str().replace([',', '_'], "");
            let value: f64 = number.parse().ok()?;
            let unit = captures.get(2)?.as_str();
            (value.is_finite() && value > 0.0).then(|| {
                (value
                    * if unit.eq_ignore_ascii_case("m") {
                        1_000_000.0
                    } else {
                        1_000.0
                    })
                .round()
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn transcript(lines: &[Value]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file
    }

    fn usage(timestamp: &str, input: u64, output: u64, read: u64, creation: u64) -> Value {
        json!({
            "timestamp": timestamp,
            "message": {
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_input_tokens": read,
                    "cache_creation_input_tokens": creation
                }
            }
        })
    }

    #[test]
    fn newest_main_chain_non_error_usage_sets_context_length() {
        let mut sidechain = usage("2026-01-01T12:00:00.000Z", 900, 1, 0, 0);
        sidechain["isSidechain"] = json!(true);
        let mut api_error = usage("2026-01-01T13:00:00.000Z", 999, 1, 0, 0);
        api_error["isApiErrorMessage"] = json!(true);
        let file = transcript(&[
            usage("2026-01-01T11:00:00.000Z", 100, 99_999, 20, 30),
            usage("2026-01-01T10:00:00.000Z", 50, 1, 5, 5),
            sidechain,
            api_error,
        ]);

        assert_eq!(transcript_context_length(file.path()).unwrap(), 150.0);
    }

    #[test]
    fn streaming_filter_keeps_finalized_and_only_final_unfinished_usage() {
        let mut finalized = usage("2026-01-01T10:00:00.000Z", 100, 1, 0, 0);
        finalized["message"]["stop_reason"] = json!("end_turn");
        let mut stale_unfinished = usage("2026-01-01T11:00:00.000Z", 200, 1, 0, 0);
        stale_unfinished["message"]["stop_reason"] = Value::Null;
        let mut latest_unfinished = usage("2026-01-01T12:00:00.000Z", 300, 1, 20, 10);
        latest_unfinished["message"]["stop_reason"] = Value::Null;
        let file = transcript(&[finalized, stale_unfinished, latest_unfinished]);

        assert_eq!(transcript_context_length(file.path()).unwrap(), 330.0);
    }

    #[test]
    fn compact_markers_and_malformed_lines_do_not_replace_usage() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "not json").unwrap();
        writeln!(
            file,
            "{}",
            usage("2026-01-01T10:00:00.000Z", 100, 7, 20, 30)
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({ "type": "system", "subtype": "compact_boundary" })
        )
        .unwrap();
        assert_eq!(transcript_context_length(file.path()).unwrap(), 150.0);

        writeln!(file, "{}", usage("2026-01-01T11:00:00.000Z", 10, 8, 2, 3)).unwrap();
        assert_eq!(transcript_context_length(file.path()).unwrap(), 15.0);
    }

    #[test]
    fn missing_empty_and_unusable_transcripts_have_zero_context() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            transcript_context_length(&directory.path().join("missing.jsonl")).unwrap(),
            0.0
        );
        assert_eq!(transcript_context_length(directory.path()).unwrap(), 0.0);
        let empty = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(transcript_context_length(empty.path()).unwrap(), 0.0);
    }

    #[test]
    fn model_context_labels_and_default_match_reference() {
        let status = |model: Value| StatusInput::from_value(json!({ "model": model })).unwrap();
        assert_eq!(
            model_context_window_size(&status(json!("Opus 4.6 (1M context)"))),
            1_000_000.0
        );
        assert_eq!(
            model_context_window_size(&status(json!({
                "id": "claude-opus",
                "display_name": "Opus 4.6 (1M)"
            }))),
            1_000_000.0
        );
        assert_eq!(
            model_context_window_size(&status(json!("claude-3-5-sonnet"))),
            200_000.0
        );
        assert_eq!(
            model_context_window_size_with_fallback(
                &status(json!("claude-3-5-sonnet")),
                Some("333333px")
            ),
            333_333.0
        );
        assert_eq!(
            model_context_window_size_with_fallback(
                &status(json!("Opus 4.6 (1M)")),
                Some("333333")
            ),
            1_000_000.0
        );
        assert_eq!(
            model_context_window_size_with_fallback(&status(json!("claude-3-5-sonnet")), Some("0")),
            200_000.0
        );
    }
}
