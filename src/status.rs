//! Loose accessors for Claude Code's status-line JSON payload.
//!
//! Keeping the original JSON value is deliberate. In particular, the thinking
//! effort widget must distinguish a missing `effort.level` from an explicit
//! `null`, and the model widget uses JavaScript nullish (not truthy) fallback
//! semantics.

use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("invalid status JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("status JSON must be an object")]
    NotAnObject,
    #[error("status JSON field {path} {message}")]
    Schema { path: String, message: &'static str },
}

#[derive(Debug, Clone)]
pub struct StatusInput {
    root: Value,
}

/// The status JSON's contribution to effort resolution.
///
/// `ExplicitDefault` is a terminal value: unlike `Missing`, it must not fall
/// through to the transcript or Claude settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusEffort<'a> {
    Missing,
    ExplicitDefault,
    Level(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextWindowMetrics {
    pub window_size: Option<f64>,
    /// Tokens occupying the context window. For object-form `current_usage`,
    /// this intentionally excludes output tokens, matching ccstatusline.
    pub context_length_tokens: Option<f64>,
    /// Total current-turn tokens, including output tokens.
    pub used_tokens: Option<f64>,
    pub used_percentage: Option<f64>,
    pub remaining_percentage: Option<f64>,
    pub total_input_tokens: Option<f64>,
    pub total_output_tokens: Option<f64>,
    pub cached_tokens: Option<f64>,
    pub total_tokens: Option<f64>,
}

impl StatusInput {
    pub fn parse(input: &[u8]) -> Result<Self, StatusError> {
        let root: Value = serde_json::from_slice(input)?;
        if !root.is_object() {
            return Err(StatusError::NotAnObject);
        }
        validate_status_schema(&root)?;
        Ok(Self { root })
    }

    #[cfg(test)]
    pub fn from_value(root: Value) -> Result<Self, StatusError> {
        if !root.is_object() {
            return Err(StatusError::NotAnObject);
        }
        validate_status_schema(&root)?;
        Ok(Self { root })
    }

    #[cfg(test)]
    pub fn raw(&self) -> &Value {
        &self.root
    }

    /// Returns only the top-level `cwd`, as required by the cwd widget.
    pub fn cwd(&self) -> Option<&str> {
        self.root.get("cwd").and_then(Value::as_str)
    }

    /// Resolves the directory used by git widgets.
    ///
    /// Git has broader fallback behavior than the cwd widget: it prefers a
    /// nonblank top-level cwd, then workspace.current_dir, then project_dir.
    pub fn git_cwd(&self) -> Option<&str> {
        [
            self.cwd(),
            self.nested_str(&["workspace", "current_dir"]),
            self.nested_str(&["workspace", "project_dir"]),
        ]
        .into_iter()
        .flatten()
        .find(|candidate| !candidate.trim().is_empty())
    }

    /// Implements `model.display_name ?? model.id`, not `display_name || id`.
    /// Therefore an explicitly empty display name is returned and suppresses
    /// the id; the widget's subsequent truthiness check hides the segment.
    pub fn model_display_name(&self) -> Option<&str> {
        match self.root.get("model")? {
            Value::String(model) => Some(model),
            Value::Object(model) => match model.get("display_name") {
                Some(Value::String(display_name)) => Some(display_name),
                Some(Value::Null) | None => model.get("id").and_then(Value::as_str),
                Some(_) => None,
            },
            _ => None,
        }
    }

    pub fn vim_mode(&self) -> Option<&str> {
        self.nested_str(&["vim", "mode"])
    }

    pub fn transcript_path(&self) -> Option<&str> {
        self.root.get("transcript_path").and_then(Value::as_str)
    }

    pub fn effort(&self) -> StatusEffort<'_> {
        let Some(Value::Object(effort)) = self.root.get("effort") else {
            return StatusEffort::Missing;
        };

        match effort.get("level") {
            Some(Value::Null) => StatusEffort::ExplicitDefault,
            Some(Value::String(level)) => StatusEffort::Level(level),
            _ => StatusEffort::Missing,
        }
    }

    pub fn context_window_metrics(&self) -> ContextWindowMetrics {
        let Some(Value::Object(window)) = self.root.get("context_window") else {
            return ContextWindowMetrics::empty();
        };

        let window_size =
            nonnegative(window.get("context_window_size")).filter(|value| *value > 0.0);
        let total_input_tokens = nonnegative(window.get("total_input_tokens"));
        let total_output_tokens = nonnegative(window.get("total_output_tokens"));

        let (used_tokens, mut context_length_tokens, cached_tokens) = match window
            .get("current_usage")
        {
            Some(Value::Object(usage)) => {
                let input = nonnegative(usage.get("input_tokens")).unwrap_or(0.0);
                let output = nonnegative(usage.get("output_tokens")).unwrap_or(0.0);
                let creation = nonnegative(usage.get("cache_creation_input_tokens")).unwrap_or(0.0);
                let read = nonnegative(usage.get("cache_read_input_tokens")).unwrap_or(0.0);

                (
                    Some(input + output + creation + read),
                    Some(input + creation + read),
                    Some(creation + read),
                )
            }
            value => {
                let scalar = nonnegative(value);
                (scalar, scalar, None)
            }
        };

        let raw_used_percentage = nonnegative(window.get("used_percentage"));
        let raw_remaining_percentage = nonnegative(window.get("remaining_percentage"));
        let used_from_percentage = raw_used_percentage
            .zip(window_size)
            .map(|(percentage, size)| percentage / 100.0 * size);
        let effective_used = used_tokens.or(used_from_percentage);
        context_length_tokens = context_length_tokens.or(effective_used);

        let used_percentage = raw_used_percentage.map(clamp_percentage).or_else(|| {
            effective_used
                .zip(window_size)
                .map(|(used, size)| clamp_percentage(used / size * 100.0))
        });
        let remaining_percentage = raw_remaining_percentage
            .map(clamp_percentage)
            .or_else(|| used_percentage.map(|percentage| 100.0 - percentage));

        let total_tokens = used_tokens.or_else(|| {
            total_input_tokens
                .zip(total_output_tokens)
                .map(|(input, output)| input + output)
        });

        ContextWindowMetrics {
            window_size,
            context_length_tokens,
            used_tokens: effective_used,
            used_percentage,
            remaining_percentage,
            total_input_tokens,
            total_output_tokens,
            cached_tokens,
            total_tokens,
        }
    }

    fn nested_str(&self, path: &[&str]) -> Option<&str> {
        let mut value = &self.root;
        for key in path {
            value = value.get(*key)?;
        }
        value.as_str()
    }
}

impl ContextWindowMetrics {
    fn empty() -> Self {
        Self {
            window_size: None,
            context_length_tokens: None,
            used_tokens: None,
            used_percentage: None,
            remaining_percentage: None,
            total_input_tokens: None,
            total_output_tokens: None,
            cached_tokens: None,
            total_tokens: None,
        }
    }
}

fn nonnegative(value: Option<&Value>) -> Option<f64> {
    let number = match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(number) if !number.trim().is_empty() => parse_javascript_number(number),
        _ => None,
    }?;

    number.is_finite().then(|| number.max(0.0))
}

/// Covers the finite forms accepted by `Number(trimmed)` that are relevant to
/// status payloads, including hexadecimal, octal, and binary strings.
fn parse_javascript_number(value: &str) -> Option<f64> {
    let value = value.trim();
    let radix_value = [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ]
    .into_iter()
    .find_map(|(prefix, radix)| {
        value.strip_prefix(prefix).and_then(|digits| {
            (!digits.is_empty())
                .then(|| u128::from_str_radix(digits, radix).ok())
                .flatten()
        })
    });

    let parsed = radix_value
        .map(|number| number as f64)
        .or_else(|| value.parse().ok())?;
    Some(parsed)
}

fn clamp_percentage(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

fn validate_status_schema(root: &Value) -> Result<(), StatusError> {
    let object = root.as_object().ok_or(StatusError::NotAnObject)?;

    for key in [
        "hook_event_name",
        "session_id",
        "transcript_path",
        "cwd",
        "version",
    ] {
        optional_string(object, key, &format!("/{key}"), false)?;
    }

    if let Some(model) = object.get("model") {
        match model {
            Value::String(_) => {}
            Value::Object(model) => {
                optional_string(model, "id", "/model/id", false)?;
                optional_string(model, "display_name", "/model/display_name", false)?;
            }
            _ => return schema_error("/model", "must be a string or object"),
        }
    }
    optional_string_object(object, "workspace", &["current_dir", "project_dir"], false)?;
    optional_string_object(object, "output_style", &["name"], false)?;

    if let Some(effort) = object.get("effort") {
        match effort {
            Value::Null => {}
            Value::Object(effort) => optional_string(effort, "level", "/effort/level", true)?,
            _ => return schema_error("/effort", "must be null or an object"),
        }
    }

    if let Some(cost) = object.get("cost") {
        let Value::Object(cost) = cost else {
            return schema_error("/cost", "must be an object");
        };
        for key in [
            "total_cost_usd",
            "total_duration_ms",
            "total_api_duration_ms",
            "total_lines_added",
            "total_lines_removed",
        ] {
            optional_number(cost, key, &format!("/cost/{key}"), false)?;
        }
    }

    if let Some(context) = object.get("context_window") {
        match context {
            Value::Null => {}
            Value::Object(context) => {
                for key in [
                    "context_window_size",
                    "total_input_tokens",
                    "total_output_tokens",
                    "used_percentage",
                    "remaining_percentage",
                ] {
                    optional_number(context, key, &format!("/context_window/{key}"), true)?;
                }
                if let Some(usage) = context.get("current_usage") {
                    match usage {
                        Value::Null => {}
                        Value::Object(usage) => {
                            for key in [
                                "input_tokens",
                                "output_tokens",
                                "cache_creation_input_tokens",
                                "cache_read_input_tokens",
                            ] {
                                optional_number(
                                    usage,
                                    key,
                                    &format!("/context_window/current_usage/{key}"),
                                    false,
                                )?;
                            }
                        }
                        value if is_coerced_number(value) => {}
                        _ => {
                            return schema_error(
                                "/context_window/current_usage",
                                "must be null, a finite number, or an object",
                            );
                        }
                    }
                }
            }
            _ => return schema_error("/context_window", "must be null or an object"),
        }
    }

    optional_string_object(object, "vim", &["mode"], true)?;
    optional_string_object(
        object,
        "worktree",
        &["name", "path", "branch", "original_cwd", "original_branch"],
        true,
    )?;
    validate_rate_limits(object)?;
    Ok(())
}

fn validate_rate_limits(object: &Map<String, Value>) -> Result<(), StatusError> {
    let Some(rate_limits) = object.get("rate_limits") else {
        return Ok(());
    };
    let rate_limits = match rate_limits {
        Value::Null => return Ok(()),
        Value::Object(rate_limits) => rate_limits,
        _ => return schema_error("/rate_limits", "must be null or an object"),
    };
    for (key, nullable) in [
        ("five_hour", false),
        ("seven_day", false),
        ("seven_day_sonnet", true),
        ("seven_day_opus", true),
    ] {
        let Some(period) = rate_limits.get(key) else {
            continue;
        };
        if period.is_null() && nullable {
            continue;
        }
        let Value::Object(period) = period else {
            return schema_error(
                &format!("/rate_limits/{key}"),
                "must be an object with nullable numeric fields",
            );
        };
        for field in ["used_percentage", "resets_at"] {
            optional_number(period, field, &format!("/rate_limits/{key}/{field}"), true)?;
        }
    }
    Ok(())
}

fn optional_string_object(
    object: &Map<String, Value>,
    key: &str,
    fields: &[&str],
    nullable: bool,
) -> Result<(), StatusError> {
    let Some(value) = object.get(key) else {
        return Ok(());
    };
    if value.is_null() && nullable {
        return Ok(());
    }
    let Value::Object(nested) = value else {
        return schema_error(&format!("/{key}"), "must be an object");
    };
    for field in fields {
        optional_string(nested, field, &format!("/{key}/{field}"), false)?;
    }
    Ok(())
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
    nullable: bool,
) -> Result<(), StatusError> {
    match object.get(key) {
        None | Some(Value::String(_)) => Ok(()),
        Some(Value::Null) if nullable => Ok(()),
        _ => schema_error(path, "must be a string"),
    }
}

fn optional_number(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
    nullable: bool,
) -> Result<(), StatusError> {
    match object.get(key) {
        None => Ok(()),
        Some(Value::Null) if nullable => Ok(()),
        Some(value) if is_coerced_number(value) => Ok(()),
        _ => schema_error(path, "must be a finite number or numeric string"),
    }
}

fn is_coerced_number(value: &Value) -> bool {
    match value {
        Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
        Value::String(number) if !number.trim().is_empty() => {
            parse_javascript_number(number).is_some_and(f64::is_finite)
        }
        _ => false,
    }
}

fn schema_error<T>(path: &str, message: &'static str) -> Result<T, StatusError> {
    Err(StatusError::Schema {
        path: path.to_string(),
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn status(value: Value) -> StatusInput {
        StatusInput::from_value(value).unwrap()
    }

    #[test]
    fn requires_an_object_but_preserves_unknown_fields() {
        assert!(matches!(
            StatusInput::parse(b"null"),
            Err(StatusError::NotAnObject)
        ));
        assert_eq!(status(json!({ "future": true })).raw()["future"], true);
    }

    #[test]
    fn git_cwd_has_distinct_nonblank_fallbacks() {
        let input = status(json!({
            "cwd": "  ",
            "workspace": { "current_dir": "/current", "project_dir": "/project" }
        }));
        assert_eq!(input.cwd(), Some("  "));
        assert_eq!(input.git_cwd(), Some("/current"));
    }

    #[test]
    fn empty_model_display_name_suppresses_the_id() {
        let input = status(json!({ "model": { "display_name": "", "id": "opus" } }));
        assert_eq!(input.model_display_name(), Some(""));

        let input = status(json!({ "model": { "id": "opus" } }));
        assert_eq!(input.model_display_name(), Some("opus"));
    }

    #[test]
    fn rejects_known_fields_that_fail_the_reference_schema() {
        assert!(matches!(
            StatusInput::parse(br#"{"model":{"display_name":null,"id":"opus"}}"#),
            Err(StatusError::Schema { ref path, .. }) if path == "/model/display_name"
        ));
        assert!(StatusInput::parse(br#"{"context_window":{"context_window_size":""}}"#).is_err());
        assert!(StatusInput::parse(br#"{"rate_limits":{"five_hour":null}}"#).is_err());
    }

    #[test]
    fn effort_distinguishes_missing_from_explicit_default() {
        assert_eq!(status(json!({})).effort(), StatusEffort::Missing);
        assert_eq!(
            status(json!({ "effort": { "level": null } })).effort(),
            StatusEffort::ExplicitDefault
        );
        assert_eq!(
            status(json!({ "effort": { "level": "HIGH" } })).effort(),
            StatusEffort::Level("HIGH")
        );
    }

    #[test]
    fn context_length_excludes_output_tokens() {
        let metrics = status(json!({
            "context_window": {
                "context_window_size": "200000",
                "current_usage": {
                    "input_tokens": "1000",
                    "output_tokens": 999,
                    "cache_creation_input_tokens": "0x10",
                    "cache_read_input_tokens": 2000
                }
            }
        }))
        .context_window_metrics();

        assert_eq!(metrics.window_size, Some(200_000.0));
        assert_eq!(metrics.context_length_tokens, Some(3_016.0));
        assert_eq!(metrics.used_tokens, Some(4_015.0));
        assert_eq!(metrics.cached_tokens, Some(2_016.0));
    }

    #[test]
    fn scalar_usage_and_percentages_follow_reference_fallbacks() {
        let metrics = status(json!({
            "context_window": {
                "context_window_size": 200,
                "current_usage": "50",
                "remaining_percentage": 120
            }
        }))
        .context_window_metrics();

        assert_eq!(metrics.context_length_tokens, Some(50.0));
        assert_eq!(metrics.used_percentage, Some(25.0));
        assert_eq!(metrics.remaining_percentage, Some(100.0));
    }
}
