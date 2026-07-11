use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub sha256: String,
    raw: Value,
    pub settings: Settings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub lines: Vec<Vec<WidgetItem>>,
    #[serde(default = "default_flex_mode")]
    pub flex_mode: String,
    #[serde(default = "default_compact_threshold")]
    #[allow(dead_code)] // Dormant while flexMode is restricted to `full`.
    pub compact_threshold: f64,
    #[serde(default = "default_color_level")]
    pub color_level: u8,
    #[allow(dead_code)] // Used only by the non-Powerline renderer.
    pub default_separator: Option<String>,
    #[serde(default)]
    pub default_padding: String,
    #[serde(default)]
    #[allow(dead_code)] // Used only by the non-Powerline renderer.
    pub inherit_separator_colors: bool,
    pub override_background_color: Option<String>,
    pub override_foreground_color: Option<String>,
    #[serde(default)]
    pub global_bold: bool,
    #[serde(default = "default_git_cache_ttl")]
    pub git_cache_ttl_seconds: f64,
    #[serde(default)]
    pub minimalist_mode: bool,
    #[serde(default)]
    pub powerline: PowerlineConfig,
    pub updatemessage: Option<Value>,
    #[allow(dead_code)] // Installation metadata does not affect rendering.
    pub installation: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PowerlineConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_separators")]
    pub separators: Vec<String>,
    #[serde(default)]
    pub separator_invert_background: Vec<bool>,
    #[serde(default)]
    pub start_caps: Vec<String>,
    #[serde(default)]
    pub end_caps: Vec<String>,
    pub theme: Option<String>,
    #[serde(default)]
    pub auto_align: bool,
    #[serde(default)]
    pub continue_theme_across_lines: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for PowerlineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            separators: default_separators(),
            separator_invert_background: vec![false],
            start_caps: Vec::new(),
            end_caps: Vec::new(),
            theme: None,
            auto_align: false,
            continue_theme_across_lines: false,
            extra: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetItem {
    #[allow(dead_code)] // Identity does not affect rendered output.
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[allow(dead_code)] // The Nord theme replaces per-widget foregrounds.
    pub color: Option<String>,
    #[allow(dead_code)] // The Nord theme replaces per-widget backgrounds.
    pub background_color: Option<String>,
    pub bold: Option<bool>,
    pub dim: Option<Value>,
    pub character: Option<String>,
    pub raw_value: Option<bool>,
    pub custom_text: Option<String>,
    pub custom_symbol: Option<String>,
    pub command_path: Option<String>,
    pub max_width: Option<u64>,
    pub preserve_colors: Option<bool>,
    pub timeout: Option<u64>,
    pub merge: Option<Value>,
    pub hide: Option<bool>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SupportIssue {
    pub path: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SupportReport {
    pub compatible_with: String,
    pub config_path: PathBuf,
    pub config_sha256: String,
    pub supported: bool,
    pub issues: Vec<SupportIssue>,
}

impl SupportReport {
    pub fn summary(&self) -> String {
        if self.supported {
            return "configuration is supported by the native renderer".to_string();
        }
        self.issues
            .iter()
            .take(3)
            .map(|issue| format!("{}: {}", issue.path, issue.message))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn markdown(&self) -> String {
        let mut output = format!(
            "Implement ccstatusline compatibility in ccstatusline-native.\n\nReference: ccstatusline {}\nConfig: {}\nConfig SHA-256: {}\n",
            crate::REFERENCE_CCSTATUSLINE_VERSION,
            self.config_path.display(),
            self.config_sha256,
        );
        for issue in &self.issues {
            output.push_str(&format!("- `{}`: {}", issue.path, issue.message));
            if let Some(value) = &issue.value {
                output.push_str(&format!("; value = `{value}`"));
            }
            output.push('\n');
        }
        output.push_str(
            "\nPlease add the smallest renderer/config capability, fixtures, and differential tests needed for these options."
        );
        output
    }
}

fn default_flex_mode() -> String {
    "full-minus-40".to_string()
}
fn default_compact_threshold() -> f64 {
    60.0
}
fn default_color_level() -> u8 {
    2
}
fn default_git_cache_ttl() -> f64 {
    5.0
}
fn default_separators() -> Vec<String> {
    vec!["\u{e0b0}".to_string()]
}

pub fn default_config_path() -> PathBuf {
    env::var_os("CCSTATUSLINE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var_os("HOME").unwrap_or_else(|| ".".into());
            PathBuf::from(home).join(".config/ccstatusline/settings.json")
        })
}

pub fn load(path: &Path) -> Result<LoadedConfig, ConfigError> {
    let bytes = fs::read(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let raw: Value = serde_json::from_slice(&bytes).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    let settings = serde_json::from_value(raw.clone()).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(LoadedConfig {
        path: path.to_path_buf(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        raw,
        settings,
    })
}

fn issue(
    path: impl Into<String>,
    message: impl Into<String>,
    value: Option<Value>,
) -> SupportIssue {
    SupportIssue {
        path: path.into(),
        message: message.into(),
        value,
    }
}

fn reject_option<T: Serialize>(
    issues: &mut Vec<SupportIssue>,
    path: &str,
    message: &str,
    value: &Option<T>,
) {
    if let Some(value) = value {
        issues.push(issue(path, message, serde_json::to_value(value).ok()));
    }
}

fn reject_private_option<T>(
    issues: &mut Vec<SupportIssue>,
    path: &str,
    message: &str,
    value: &Option<T>,
) {
    if value.is_some() {
        issues.push(issue(path, message, None));
    }
}

pub fn support_report(config: &LoadedConfig) -> SupportReport {
    let settings = &config.settings;
    let mut issues = Vec::new();
    validate_reference_schema(config, &mut issues);

    if settings.version != 3 {
        issues.push(issue(
            "/version",
            "only fully migrated v3 settings are supported",
            Some(Value::from(settings.version)),
        ));
    }
    if settings.lines.is_empty() {
        issues.push(issue("/lines", "at least one line is required", None));
    }
    if settings.flex_mode != "full" {
        issues.push(issue(
            "/flexMode",
            "only flexMode=full is implemented",
            Some(Value::from(settings.flex_mode.clone())),
        ));
    }
    if settings.color_level != 3 {
        issues.push(issue(
            "/colorLevel",
            "only truecolor level 3 is implemented",
            Some(Value::from(settings.color_level)),
        ));
    }
    let uses_git_summary = settings.lines.iter().flatten().any(|item| {
        item.kind == "custom-command"
            && item.command_path.as_deref() == Some(crate::git::GIT_SUMMARY_COMMAND)
    });
    if uses_git_summary && settings.git_cache_ttl_seconds != 5.0 {
        issues.push(issue(
            "/gitCacheTtlSeconds",
            "the native fast path currently implements only the five-second Git cache used by the intrinsic helper",
            serde_json::Number::from_f64(settings.git_cache_ttl_seconds).map(Value::Number),
        ));
    }
    if settings.global_bold {
        issues.push(issue(
            "/globalBold",
            "global bold rendering is not implemented",
            Some(Value::Bool(true)),
        ));
    }
    if settings.minimalist_mode {
        issues.push(issue(
            "/minimalistMode",
            "minimalist raw-value override is not implemented",
            Some(Value::Bool(true)),
        ));
    }
    for (path, value) in [
        (
            "/overrideBackgroundColor",
            settings.override_background_color.as_ref(),
        ),
        (
            "/overrideForegroundColor",
            settings.override_foreground_color.as_ref(),
        ),
    ] {
        if value.is_some_and(|value| !value.is_empty() && value != "none") {
            issues.push(issue(
                path,
                "global color overrides are not implemented",
                value.cloned().map(Value::from),
            ));
        }
    }
    if settings.updatemessage.is_some() {
        issues.push(issue(
            "/updatemessage",
            "update messages mutate settings and must use the reference fallback",
            None,
        ));
    }
    for key in settings.extra.keys() {
        issues.push(issue(
            format!("/{key}"),
            "unknown top-level setting may affect rendering",
            None,
        ));
    }

    let powerline = &settings.powerline;
    if !powerline.enabled {
        issues.push(issue(
            "/powerline/enabled",
            "only Powerline rendering is implemented",
            Some(Value::Bool(false)),
        ));
    }
    if powerline.theme.as_deref() != Some("nord") {
        issues.push(issue(
            "/powerline/theme",
            "only the nord theme is implemented",
            powerline.theme.clone().map(Value::from),
        ));
    }
    if powerline.auto_align {
        issues.push(issue(
            "/powerline/autoAlign",
            "cross-line auto alignment is not implemented",
            Some(Value::Bool(true)),
        ));
    }
    if powerline.continue_theme_across_lines {
        issues.push(issue(
            "/powerline/continueThemeAcrossLines",
            "continued theme indexing is not implemented",
            Some(Value::Bool(true)),
        ));
    }
    if powerline.separators.is_empty() {
        issues.push(issue(
            "/powerline/separators",
            "at least one separator is required",
            None,
        ));
    }
    for (field, values) in [
        ("separators", &powerline.separators),
        ("startCaps", &powerline.start_caps),
        ("endCaps", &powerline.end_caps),
    ] {
        for (index, value) in values.iter().enumerate() {
            if crate::ansi::requires_reference_width(value) {
                issues.push(issue(
                    format!("/powerline/{field}/{index}"),
                    "this Unicode sequence uses the reference width engine",
                    None,
                ));
            }
        }
    }
    if crate::ansi::requires_reference_width(&settings.default_padding) {
        issues.push(issue(
            "/defaultPadding",
            "this Unicode sequence uses the reference width engine",
            None,
        ));
    }
    for key in powerline.extra.keys() {
        issues.push(issue(
            format!("/powerline/{key}"),
            "unknown Powerline setting may affect rendering",
            None,
        ));
    }

    for (line_index, line) in settings.lines.iter().enumerate() {
        for (item_index, item) in line.iter().enumerate() {
            validate_widget(item, line_index, item_index, &mut issues);
        }
    }

    SupportReport {
        compatible_with: format!("ccstatusline@{}", crate::REFERENCE_CCSTATUSLINE_VERSION),
        config_path: config.path.clone(),
        config_sha256: config.sha256.clone(),
        supported: issues.is_empty(),
        issues,
    }
}

fn validate_reference_schema(config: &LoadedConfig, issues: &mut Vec<SupportIssue>) {
    let Some(root) = config.raw.as_object() else {
        issues.push(issue("/", "settings must be a JSON object", None));
        return;
    };

    if !(1.0..=99.0).contains(&config.settings.compact_threshold) {
        issues.push(issue(
            "/compactThreshold",
            "ccstatusline requires a number from 1 through 99",
            serde_json::Number::from_f64(config.settings.compact_threshold).map(Value::Number),
        ));
    }
    if !(0.0..=60.0).contains(&config.settings.git_cache_ttl_seconds) {
        issues.push(issue(
            "/gitCacheTtlSeconds",
            "ccstatusline requires a number from 0 through 60",
            serde_json::Number::from_f64(config.settings.git_cache_ttl_seconds).map(Value::Number),
        ));
    }

    for key in [
        "defaultSeparator",
        "overrideBackgroundColor",
        "overrideForegroundColor",
        "updatemessage",
        "installation",
    ] {
        if root.get(key).is_some_and(Value::is_null) {
            issues.push(issue(
                format!("/{key}"),
                "explicit null is invalid in ccstatusline's settings schema",
                None,
            ));
        }
    }

    if let Some(installation) = root.get("installation") {
        if !installation.is_null() && !valid_installation(installation) {
            issues.push(issue(
                "/installation",
                "installation metadata does not match ccstatusline's schema",
                None,
            ));
        }
    }

    if root
        .get("powerline")
        .and_then(Value::as_object)
        .and_then(|powerline| powerline.get("theme"))
        .is_some_and(Value::is_null)
    {
        issues.push(issue(
            "/powerline/theme",
            "explicit null is invalid in ccstatusline's settings schema",
            None,
        ));
    }

    const NULLABLE_WIDGET_FIELDS: &[&str] = &[
        "color",
        "backgroundColor",
        "bold",
        "dim",
        "character",
        "rawValue",
        "customText",
        "customSymbol",
        "commandPath",
        "maxWidth",
        "preserveColors",
        "timeout",
        "merge",
        "hide",
    ];
    let Some(lines) = root.get("lines").and_then(Value::as_array) else {
        return;
    };
    for (line_index, line) in lines.iter().filter_map(Value::as_array).enumerate() {
        for (item_index, item) in line.iter().filter_map(Value::as_object).enumerate() {
            for key in NULLABLE_WIDGET_FIELDS {
                if item.get(*key).is_some_and(Value::is_null) {
                    issues.push(issue(
                        format!("/lines/{line_index}/{item_index}/{key}"),
                        "explicit null is invalid in ccstatusline's widget schema",
                        None,
                    ));
                }
            }
        }
    }
}

fn valid_installation(value: &Value) -> bool {
    let Some(metadata) = value.as_object() else {
        return false;
    };
    match metadata.get("method").and_then(Value::as_str) {
        Some("auto-update") => matches!(
            metadata.get("packageManager").and_then(Value::as_str),
            Some("npm" | "bun")
        ),
        Some("pinned") => metadata
            .get("installedVersion")
            .is_none_or(Value::is_string),
        Some("self-managed" | "unknown") => metadata
            .get("packageManager")
            .is_none_or(|manager| matches!(manager.as_str(), Some("npm" | "bun" | "unknown"))),
        _ => false,
    }
}

fn validate_widget(
    item: &WidgetItem,
    line_index: usize,
    item_index: usize,
    issues: &mut Vec<SupportIssue>,
) {
    let base = format!("/lines/{line_index}/{item_index}");
    const SUPPORTED: &[&str] = &[
        "vim-mode",
        "context-bar",
        "flex-separator",
        "model",
        "thinking-effort",
        "current-working-dir",
        "git-branch",
        "custom-command",
    ];
    if !SUPPORTED.contains(&item.kind.as_str()) {
        issues.push(issue(
            format!("{base}/type"),
            format!("unsupported widget `{}`", item.kind),
            Some(Value::from(item.kind.clone())),
        ));
        return;
    }

    if item.bold.unwrap_or(false) {
        issues.push(issue(
            format!("{base}/bold"),
            "per-widget bold is not implemented",
            Some(Value::Bool(true)),
        ));
    }
    if item
        .dim
        .as_ref()
        .is_some_and(|value| value != &Value::Bool(false))
    {
        issues.push(issue(
            format!("{base}/dim"),
            "per-widget dim is not implemented",
            item.dim.clone(),
        ));
    }
    if item
        .merge
        .as_ref()
        .is_some_and(|value| value != &Value::Bool(false))
    {
        issues.push(issue(
            format!("{base}/merge"),
            "widget merging is not implemented",
            item.merge.clone(),
        ));
    }
    if item.hide.unwrap_or(false) {
        issues.push(issue(
            format!("{base}/hide"),
            "generic widget hiding is not implemented",
            Some(Value::Bool(true)),
        ));
    }
    reject_option(
        issues,
        &format!("{base}/character"),
        "custom layout characters are not implemented",
        &item.character,
    );
    reject_private_option(
        issues,
        &format!("{base}/customText"),
        "custom text is not valid for this widget",
        &item.custom_text,
    );
    reject_private_option(
        issues,
        &format!("{base}/customSymbol"),
        "custom symbols are not implemented",
        &item.custom_symbol,
    );
    if item.kind == "custom-command" {
        if item.command_path.as_deref() != Some(crate::git::GIT_SUMMARY_COMMAND) {
            issues.push(issue(
                format!("{base}/commandPath"),
                format!(
                    "only the intrinsic `{}` custom command is implemented",
                    crate::git::GIT_SUMMARY_COMMAND
                ),
                None,
            ));
        }
    } else {
        reject_private_option(
            issues,
            &format!("{base}/commandPath"),
            "custom commands are not implemented",
            &item.command_path,
        );
    }
    reject_option(
        issues,
        &format!("{base}/maxWidth"),
        "per-widget truncation is not implemented",
        &item.max_width,
    );
    if item.kind == "custom-command" {
        if item.preserve_colors == Some(true) {
            issues.push(issue(
                format!("{base}/preserveColors"),
                "preserved child colors are not implemented for the intrinsic Git summary",
                Some(Value::Bool(true)),
            ));
        }
        if item.timeout.is_some_and(|timeout| timeout != 1000) {
            issues.push(issue(
                format!("{base}/timeout"),
                "the intrinsic Git summary supports only the ccstatusline default timeout of 1000ms",
                item.timeout.map(Value::from),
            ));
        }
    } else {
        reject_option(
            issues,
            &format!("{base}/preserveColors"),
            "preserved child colors are not implemented",
            &item.preserve_colors,
        );
        reject_option(
            issues,
            &format!("{base}/timeout"),
            "widget command timeouts are not implemented",
            &item.timeout,
        );
    }
    for key in item.extra.keys() {
        issues.push(issue(
            format!("{base}/{key}"),
            "unknown widget option may affect rendering",
            None,
        ));
    }

    if item.kind == "flex-separator" && item.raw_value.is_some() {
        issues.push(issue(
            format!("{base}/rawValue"),
            "rawValue is invalid for flex separators",
            item.raw_value.map(Value::Bool),
        ));
    }
    if item.kind == "custom-command" && item.raw_value.is_some() {
        issues.push(issue(
            format!("{base}/rawValue"),
            "rawValue must be absent for the intrinsic Git summary",
            item.raw_value.map(Value::Bool),
        ));
    }

    match item.kind.as_str() {
        "vim-mode" => {
            for (key, value) in &item.metadata {
                let valid = match key.as_str() {
                    "format" => matches!(
                        value.as_str(),
                        "icon-dash-letter" | "icon-letter" | "icon" | "letter" | "word"
                    ),
                    "nerdFont" => matches!(value.as_str(), "true" | "false"),
                    _ => false,
                };
                if !valid {
                    issues.push(issue(
                        format!("{base}/metadata/{key}"),
                        "unsupported Vim-mode metadata",
                        Some(Value::from(value.clone())),
                    ));
                }
            }
        }
        "context-bar" => {
            for (key, value) in &item.metadata {
                if key != "display" || !matches!(value.as_str(), "progress-short" | "progress") {
                    issues.push(issue(
                        format!("{base}/metadata/{key}"),
                        "only progress-short and progress context displays are implemented",
                        Some(Value::from(value.clone())),
                    ));
                }
            }
        }
        _ if !item.metadata.is_empty() => {
            for (key, value) in &item.metadata {
                issues.push(issue(
                    format!("{base}/metadata/{key}"),
                    "metadata for this widget is not implemented",
                    Some(Value::from(value.clone())),
                ));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current_settings() -> LoadedConfig {
        let settings: Settings =
            serde_json::from_str(include_str!("../tests/fixtures/settings.json")).unwrap();
        LoadedConfig {
            path: PathBuf::from("settings.json"),
            sha256: format!(
                "{:x}",
                Sha256::digest(include_bytes!("../tests/fixtures/settings.json"))
            ),
            raw: serde_json::from_str(include_str!("../tests/fixtures/settings.json")).unwrap(),
            settings,
        }
    }

    #[test]
    fn current_fixture_is_supported() {
        let report = support_report(&current_settings());
        assert_eq!(report.issues, Vec::new());
        assert!(report.supported);
    }

    #[test]
    fn requires_the_intrinsic_helpers_five_second_git_cache() {
        let mut config = current_settings();
        {
            let item = &mut config.settings.lines[1][1];
            item.kind = "custom-command".into();
            item.command_path = Some(crate::git::GIT_SUMMARY_COMMAND.into());
        }
        config.settings.git_cache_ttl_seconds = 1.0;
        let report = support_report(&config);
        assert!(!report.supported);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.path == "/gitCacheTtlSeconds")
        );

        let mut branch_only = current_settings();
        branch_only.settings.git_cache_ttl_seconds = 1.0;
        assert!(support_report(&branch_only).supported);
    }

    #[test]
    fn reports_every_unsupported_widget() {
        let mut config = current_settings();
        config.settings.lines[0][0].kind = "session-cost".into();
        config.settings.lines[1][1].kind = "custom-command".into();
        let report = support_report(&config);
        assert_eq!(report.issues.len(), 2);
        assert!(report.markdown().contains("session-cost"));
        assert!(report.markdown().contains("/lines/1/1/commandPath"));
    }

    #[test]
    fn supports_only_the_intrinsic_git_summary_custom_command() {
        let mut config = current_settings();
        {
            let item = &mut config.settings.lines[1][1];
            item.kind = "custom-command".into();
            item.command_path = Some(crate::git::GIT_SUMMARY_COMMAND.into());
        }
        assert!(support_report(&config).supported);

        config.settings.lines[1][1].command_path = Some("printf arbitrary-command".into());
        let report = support_report(&config);
        assert!(!report.supported);
        assert_eq!(report.issues[0].path, "/lines/1/1/commandPath");
        assert!(!report.markdown().contains("printf arbitrary-command"));
    }

    #[test]
    fn intrinsic_git_summary_accepts_only_compatible_command_options() {
        let mut config = current_settings();
        {
            let item = &mut config.settings.lines[1][1];
            item.kind = "custom-command".into();
            item.command_path = Some(crate::git::GIT_SUMMARY_COMMAND.into());
            item.preserve_colors = Some(false);
            item.timeout = Some(1000);
        }
        assert!(support_report(&config).supported);

        {
            let item = &mut config.settings.lines[1][1];
            item.raw_value = Some(false);
            item.max_width = Some(80);
            item.preserve_colors = Some(true);
            item.timeout = Some(999);
            item.metadata.insert("future".into(), "value".into());
        }
        let paths = support_report(&config)
            .issues
            .into_iter()
            .map(|issue| issue.path)
            .collect::<Vec<_>>();
        assert!(paths.contains(&"/lines/1/1/rawValue".into()));
        assert!(paths.contains(&"/lines/1/1/maxWidth".into()));
        assert!(paths.contains(&"/lines/1/1/preserveColors".into()));
        assert!(paths.contains(&"/lines/1/1/timeout".into()));
        assert!(paths.contains(&"/lines/1/1/metadata/future".into()));
    }

    #[test]
    fn report_does_not_copy_arbitrary_config_contents() {
        let mut config = current_settings();
        config.settings.lines[0][0].custom_text = Some("private-widget-text".into());
        config.settings.extra.insert(
            "futureSetting".into(),
            Value::String("private-setting-value".into()),
        );
        let report = support_report(&config).markdown();
        assert!(report.contains("/lines/0/0/customText"));
        assert!(report.contains("/futureSetting"));
        assert!(!report.contains("private-widget-text"));
        assert!(!report.contains("private-setting-value"));
    }

    #[test]
    fn rejects_values_that_make_reference_config_parsing_fail() {
        let cases = [
            ("overrideBackgroundColor", Value::Null),
            ("gitCacheTtlSeconds", Value::from(61)),
            ("installation", Value::from(42)),
        ];
        for (key, value) in cases {
            let mut config = current_settings();
            config.raw[key] = value.clone();
            config.settings = serde_json::from_value(config.raw.clone()).unwrap();
            assert!(!support_report(&config).supported, "accepted {key}={value}");
        }

        let mut config = current_settings();
        config.raw["lines"][0][0]["color"] = Value::Null;
        config.settings = serde_json::from_value(config.raw.clone()).unwrap();
        assert!(!support_report(&config).supported);
    }

    #[test]
    fn rejects_width_sequences_not_shared_with_reference_engine() {
        let mut config = current_settings();
        config.settings.powerline.start_caps[0] = "क्‍ष".into();
        assert!(!support_report(&config).supported);
    }
}
