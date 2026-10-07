use crate::ansi::{truncate_styled, visible_text, visible_width};
use crate::config::{Settings, WidgetItem};
use crate::git::GitResolver;
use crate::status::StatusInput;
use std::collections::HashMap;

const FLEX_SENTINEL: &str = "\x01FLEX_SEP\x01";

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("reference renderer required: {0}")]
    NeedsFallback(String),
}

#[derive(Debug, Clone)]
struct PreRendered {
    content: String,
}

#[derive(Debug)]
struct Element {
    content: String,
    foreground: Rgb,
    background: Rgb,
    original_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb(u8, u8, u8);

const NORD_FOREGROUNDS: [Rgb; 5] = [
    Rgb(46, 52, 64),
    Rgb(216, 222, 233),
    Rgb(253, 246, 227),
    Rgb(46, 52, 64),
    Rgb(46, 52, 64),
];
const NORD_BACKGROUNDS: [Rgb; 5] = [
    Rgb(136, 192, 208),
    Rgb(76, 86, 106),
    Rgb(94, 129, 172),
    Rgb(180, 142, 173),
    Rgb(163, 190, 140),
];

pub fn render(
    settings: &Settings,
    status: &StatusInput,
    detected_width: Option<usize>,
) -> Result<String, RenderError> {
    let mut git = GitResolver::new(settings.git_cache_ttl_seconds);
    let mut pre_rendered_lines = Vec::with_capacity(settings.lines.len());
    for line in &settings.lines {
        let mut rendered = Vec::with_capacity(line.len());
        for item in line {
            let content = if item.kind == "flex-separator" {
                String::new()
            } else {
                crate::widgets::render(item, status, &mut git)?.unwrap_or_default()
            };
            if crate::ansi::requires_reference_width(&content) {
                return Err(RenderError::NeedsFallback(
                    "widget output contains a Unicode sequence handled by the reference width engine"
                        .into(),
                ));
            }
            rendered.push(PreRendered { content });
        }
        pre_rendered_lines.push(rendered);
    }

    let mut output = String::new();
    let mut global_separator_offset = 0;
    let mut global_cap_offset = 0;
    for (line, pre_rendered) in settings.lines.iter().zip(&pre_rendered_lines) {
        let rendered = render_powerline_line(
            line,
            pre_rendered,
            settings,
            detected_width,
            global_separator_offset,
            global_cap_offset,
        );
        if visible_text(&rendered).trim().is_empty() {
            continue;
        }
        output.push_str("\x1b[0m");
        output.push_str(&rendered.replace(' ', "\u{a0}"));
        output.push('\n');
        global_separator_offset += count_separator_slots(line, pre_rendered);
        global_cap_offset += count_cap_slots(line, pre_rendered);
    }
    Ok(output)
}

fn render_powerline_line(
    widgets: &[WidgetItem],
    pre_rendered: &[PreRendered],
    settings: &Settings,
    detected_width: Option<usize>,
    global_separator_offset: usize,
    global_cap_offset: usize,
) -> String {
    let mut elements = Vec::new();
    let mut color_index = 0;
    let (leading_padding, trailing_padding) = padding_sides(settings);
    for (original_index, (widget, rendered)) in widgets.iter().zip(pre_rendered).enumerate() {
        if widget.kind == "flex-separator" || rendered.content.is_empty() {
            continue;
        }
        let theme_index = color_index % NORD_FOREGROUNDS.len();
        color_index += 1;
        elements.push(Element {
            content: format!("{leading_padding}{}{trailing_padding}", rendered.content),
            foreground: NORD_FOREGROUNDS[theme_index],
            background: NORD_BACKGROUNDS[theme_index],
            original_index,
        });
    }
    if elements.is_empty() {
        return String::new();
    }

    let rendered_index_by_original = elements
        .iter()
        .enumerate()
        .map(|(rendered_index, element)| (element.original_index, rendered_index))
        .collect::<HashMap<_, _>>();
    let mut flex_after_index = HashMap::<usize, usize>::new();
    let mut start_cap_before_index = HashMap::<usize, usize>::new();
    let mut segment_offset_by_index = HashMap::<usize, usize>::new();
    let mut leading_flex_count = 0;
    let mut total_flex_count = 0;
    let mut last_rendered_index = None;
    let mut pending_flex_count = 0;
    let mut segment_offset = 0;
    let mut has_rendered_segment = false;

    for (original_index, widget) in widgets.iter().enumerate() {
        if widget.kind == "flex-separator" {
            total_flex_count += 1;
            if let Some(last) = last_rendered_index {
                pending_flex_count += 1;
                *flex_after_index.entry(last).or_default() += 1;
            } else {
                leading_flex_count += 1;
            }
            continue;
        }
        let Some(&rendered_index) = rendered_index_by_original.get(&original_index) else {
            continue;
        };
        if !has_rendered_segment {
            start_cap_before_index.insert(rendered_index, segment_offset);
            pending_flex_count = 0;
            has_rendered_segment = true;
        } else if pending_flex_count > 0 {
            segment_offset += 1;
            start_cap_before_index.insert(rendered_index, segment_offset);
            pending_flex_count = 0;
        }
        segment_offset_by_index.insert(rendered_index, segment_offset);
        last_rendered_index = Some(rendered_index);
    }

    let mut result = FLEX_SENTINEL.repeat(leading_flex_count);
    let mut local_separator_index = 0;
    for (index, widget) in elements.iter().enumerate() {
        let next = elements.get(index + 1);
        let current_segment = *segment_offset_by_index.get(&index).unwrap_or(&0);
        let flex_count_after = *flex_after_index.get(&index).unwrap_or(&0);

        if let Some(start_segment) = start_cap_before_index.get(&index).copied() {
            let cap = cap_at(
                &settings.powerline.start_caps,
                global_cap_offset + start_segment,
            );
            if !cap.is_empty() {
                result.push_str(&fg(widget.background));
                result.push_str(cap);
                result.push_str("\x1b[39m");
            }
        }

        result.push_str(&fg(widget.foreground));
        result.push_str(&bg(widget.background));
        result.push_str(&widget.content);
        result.push_str("\x1b[49m\x1b[39m");

        if flex_count_after > 0 {
            let cap = cap_at(
                &settings.powerline.end_caps,
                global_cap_offset + current_segment,
            );
            if !cap.is_empty() {
                result.push_str(&fg(widget.background));
                result.push_str(cap);
                result.push_str("\x1b[39m");
            }
            result.push_str(&FLEX_SENTINEL.repeat(flex_count_after));
            continue;
        }

        if let Some(next) = next {
            let separator_index = global_separator_offset + local_separator_index;
            let separator_slot = separator_index % settings.powerline.separators.len();
            let separator = &settings.powerline.separators[separator_slot];
            let inverted = settings
                .powerline
                .separator_invert_background
                .get(separator_slot)
                .copied()
                .unwrap_or(false);
            result.push_str(&render_separator(widget, next, separator, inverted));
            local_separator_index += 1;
        }
    }

    let last_index = elements.len() - 1;
    if flex_after_index.get(&last_index).copied().unwrap_or(0) == 0 {
        let last = &elements[last_index];
        let current_segment = *segment_offset_by_index.get(&last_index).unwrap_or(&0);
        let cap = cap_at(
            &settings.powerline.end_caps,
            global_cap_offset + current_segment,
        );
        if !cap.is_empty() {
            result.push_str(&fg(last.background));
            result.push_str(cap);
            result.push_str("\x1b[39m");
        }
    }

    let effective_width = detected_width.and_then(|width| width.checked_sub(6));
    if total_flex_count > 0 {
        if let Some(width) = effective_width.filter(|width| *width > 0) {
            let parts = result.split(FLEX_SENTINEL).collect::<Vec<_>>();
            let content_width = parts.iter().map(|part| visible_width(part)).sum::<usize>();
            let flex_count = parts.len().saturating_sub(1);
            let available = width.saturating_sub(content_width);
            let per_flex = available / flex_count.max(1);
            let remainder = available % flex_count.max(1);
            let mut expanded = parts.first().copied().unwrap_or_default().to_string();
            for (index, part) in parts.iter().skip(1).enumerate() {
                expanded.push_str(&" ".repeat(per_flex + usize::from(index < remainder)));
                expanded.push_str(part);
            }
            result = expanded;
        } else {
            result = result.replace(FLEX_SENTINEL, " ");
        }
    }

    if let Some(width) = effective_width.filter(|width| *width > 0) {
        if visible_width(&result) > width {
            result = truncate_styled(&result, width);
        }
    }
    result
}

/// ccstatusline 2.2.30 resolvePaddingSides: the validator restricts the value
/// to `both`, `left`, or `right`, so any other string falls through to `both`.
fn padding_sides(settings: &Settings) -> (&str, &str) {
    match settings.default_padding_side.as_str() {
        "left" => (settings.default_padding.as_str(), ""),
        "right" => ("", settings.default_padding.as_str()),
        _ => (
            settings.default_padding.as_str(),
            settings.default_padding.as_str(),
        ),
    }
}

fn render_separator(previous: &Element, next: &Element, text: &str, inverted: bool) -> String {
    let same_background = previous.background == next.background;
    let (foreground, background) = if inverted {
        (
            if same_background {
                next.foreground
            } else {
                next.background
            },
            previous.background,
        )
    } else {
        (
            if same_background {
                previous.foreground
            } else {
                previous.background
            },
            next.background,
        )
    };
    format!(
        "{}{}{}\x1b[39m\x1b[49m",
        fg(foreground),
        bg(background),
        text
    )
}

fn cap_at(caps: &[String], index: usize) -> &str {
    if caps.is_empty() {
        ""
    } else {
        &caps[index % caps.len()]
    }
}

fn count_separator_slots(widgets: &[WidgetItem], rendered: &[PreRendered]) -> usize {
    let mut count = 0;
    let mut has_previous = false;
    for (index, widget) in widgets.iter().enumerate() {
        if widget.kind == "flex-separator" {
            has_previous = false;
            continue;
        }
        if rendered
            .get(index)
            .is_none_or(|entry| entry.content.is_empty())
        {
            continue;
        }
        if has_previous {
            count += 1;
        }
        has_previous = true;
    }
    count
}

fn count_cap_slots(widgets: &[WidgetItem], rendered: &[PreRendered]) -> usize {
    let mut has_segment = false;
    let mut pending_flex = false;
    let mut count = 0;
    for (index, widget) in widgets.iter().enumerate() {
        if widget.kind == "flex-separator" {
            if has_segment {
                pending_flex = true;
            }
            continue;
        }
        if rendered
            .get(index)
            .is_none_or(|entry| entry.content.is_empty())
        {
            continue;
        }
        if !has_segment {
            has_segment = true;
            count = 1;
        } else if pending_flex {
            count += 1;
            pending_flex = false;
        }
    }
    count
}

fn fg(Rgb(red, green, blue): Rgb) -> String {
    format!("\x1b[38;2;{red};{green};{blue}m")
}

fn bg(Rgb(red, green, blue): Rgb) -> String {
    format!("\x1b[48;2;{red};{green};{blue}m")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn current_surface_cases() -> [(usize, &'static str); 6] {
        [
            (
                60,
                "5d3f5dc862cc964768c9b1a0d919d35322899489c2fcd70b939a3984433100df",
            ),
            (
                70,
                "547b7befa33ced8862589dd5b9ffb6685dea6961f6150833c7360896f4d72f35",
            ),
            (
                80,
                "66f2583e26ffd5aeae8121344c8a934fa3e4b8892563c28ae67138d43b3f3ebc",
            ),
            (
                100,
                "d4c42192a28e49d25c61b11a4705bd66bd3f1d20f2a4558a8282a2782c786ce1",
            ),
            (
                120,
                "bcd619783ccc4169b5a0c36c3bb5c4a1e1169847a01b3a19bf39cdb8d0749017",
            ),
            (
                160,
                "9149d127a035603917918e030964fbbe3a3194fbb3004d257a2b14a9c6c2159a",
            ),
        ]
    }

    #[test]
    fn current_surface_matches_pinned_oracle_at_multiple_widths() {
        let settings: Settings =
            serde_json::from_str(include_str!("../tests/fixtures/settings.json")).unwrap();
        let status = StatusInput::parse(include_bytes!("../tests/fixtures/status.json")).unwrap();
        for (width, expected) in current_surface_cases() {
            let output = render(&settings, &status, Some(width)).unwrap();
            let actual = format!("{:x}", Sha256::digest(output.as_bytes()));
            assert_eq!(actual, expected, "oracle mismatch at width {width}");
        }
    }

    #[test]
    fn rich_git_intrinsic_matches_pinned_custom_command_oracle() {
        let settings: Settings =
            serde_json::from_str(include_str!("../tests/fixtures/settings-git-summary.json"))
                .unwrap();
        let status = StatusInput::parse(include_bytes!("../tests/fixtures/status.json")).unwrap();
        let cases = [
            (
                80,
                "66f2583e26ffd5aeae8121344c8a934fa3e4b8892563c28ae67138d43b3f3ebc",
            ),
            (
                131,
                "c33bf207ab26ea22b48f416c9fa9753b32f904fc61e4a7fbdc44a8129f3a0766",
            ),
            (
                200,
                "03799acc335e69780fbb4555de82c8d4f97a1629907583c645c569a81f4433f0",
            ),
        ];

        for (width, expected) in cases {
            let output = render(&settings, &status, Some(width)).unwrap();
            let actual = format!("{:x}", Sha256::digest(output.as_bytes()));
            assert_eq!(
                actual, expected,
                "rich Git oracle mismatch at width {width}"
            );
        }
    }

    #[test]
    fn v4_fixtures_match_the_pinned_oracle() {
        let status = StatusInput::parse(include_bytes!("../tests/fixtures/status.json")).unwrap();
        // A load-time 3->4 migration and a full TUI save (materialized
        // defaultPaddingSide and cache TTLs) render exactly like the v3
        // fixture, so they share its oracle hashes.
        let standard_cases = current_surface_cases();
        let fixtures: [(&str, Settings); 2] = [
            (
                "settings-v4.json",
                serde_json::from_str(include_str!("../tests/fixtures/settings-v4.json")).unwrap(),
            ),
            (
                "settings-v4-saved.json",
                serde_json::from_str(include_str!("../tests/fixtures/settings-v4-saved.json"))
                    .unwrap(),
            ),
        ];
        for (name, settings) in &fixtures {
            for (width, expected) in standard_cases {
                let output = render(settings, &status, Some(width)).unwrap();
                let actual = format!("{:x}", Sha256::digest(output.as_bytes()));
                assert_eq!(actual, expected, "{name} oracle mismatch at width {width}");
            }
        }

        let intrinsic: Settings = serde_json::from_str(include_str!(
            "../tests/fixtures/settings-v4-git-summary.json"
        ))
        .unwrap();
        for (width, expected) in [
            (
                80,
                "66f2583e26ffd5aeae8121344c8a934fa3e4b8892563c28ae67138d43b3f3ebc",
            ),
            (
                131,
                "c33bf207ab26ea22b48f416c9fa9753b32f904fc61e4a7fbdc44a8129f3a0766",
            ),
            (
                200,
                "03799acc335e69780fbb4555de82c8d4f97a1629907583c645c569a81f4433f0",
            ),
        ] {
            let output = render(&intrinsic, &status, Some(width)).unwrap();
            let actual = format!("{:x}", Sha256::digest(output.as_bytes()));
            assert_eq!(
                actual, expected,
                "v4 intrinsic oracle mismatch at width {width}"
            );
        }

        // The status fixture's working directory is not a repository, so the
        // migrated `hide: "no-git"` list drops the git-branch segment.
        let hidden: Settings = serde_json::from_str(include_str!(
            "../tests/fixtures/settings-v4-hide-nogit.json"
        ))
        .unwrap();
        for (width, expected) in [
            (
                60,
                "b1ae7534299286799be6bdd627ba6611caa7b95da8e62d6c65e08202f190361a",
            ),
            (
                70,
                "4510e7851e8ba3998ed91b9d8a6553e39dacf6355a39ba2ea90dce638bc97d6b",
            ),
            (
                80,
                "69de4f22e3c6597e449fbcc1d7250c590b9c2dfc5fdee2b231f2c528aef88778",
            ),
            (
                100,
                "06b2f02b09f09ef3b7b9ee92848722b2c828135d64b06c6d398fc32ee5d8ec18",
            ),
            (
                120,
                "78a2fb679f0fd51b742b9b28d864a901072b56cb6f3d3b6e41c291b48656dc35",
            ),
            (
                160,
                "f03ff0354204f430b7772d5cc3307c82e9efe2afdbe90da5b1a70c6b0008c98e",
            ),
        ] {
            let output = render(&hidden, &status, Some(width)).unwrap();
            let actual = format!("{:x}", Sha256::digest(output.as_bytes()));
            assert_eq!(
                actual, expected,
                "hide no-git oracle mismatch at width {width}"
            );
        }
    }

    #[test]
    fn one_sided_default_padding_matches_the_pinned_oracle() {
        let base: Settings =
            serde_json::from_str(include_str!("../tests/fixtures/settings-v4.json")).unwrap();
        let status = StatusInput::parse(include_bytes!("../tests/fixtures/status.json")).unwrap();
        let cases = [
            (
                "left",
                [
                    (
                        60,
                        "d5f8c7cf1c8082a1ca80464b7c010d19cbd0e32fe161bb01de6fc9a5e5e6f707",
                    ),
                    (
                        80,
                        "86b065265847f9d773ddac72adf87ebf0f38be1880f9170d44596c6764b46692",
                    ),
                    (
                        160,
                        "40deac6fac81b63da6846c56f7d698cca48fbb649593993a95daf6458fdf9db7",
                    ),
                ],
            ),
            (
                "right",
                [
                    (
                        60,
                        "31dd332a8902d54f5e57c45593fa2ac5371f42f7e55ad3e27ca225c53d52b3d2",
                    ),
                    (
                        80,
                        "9ff5ff263bc66dcb610051168eb07e1387a23d214e57771f1b808d2790ab855e",
                    ),
                    (
                        160,
                        "d3122f9e49df7bf6d8f07aa951200a57d5d16af06b964e87fde0888181819a7c",
                    ),
                ],
            ),
        ];
        for (side, hashes) in cases {
            let mut settings = base.clone();
            settings.default_padding_side = side.to_string();
            for (width, expected) in hashes {
                let output = render(&settings, &status, Some(width)).unwrap();
                let actual = format!("{:x}", Sha256::digest(output.as_bytes()));
                assert_eq!(
                    actual, expected,
                    "padding side {side} oracle mismatch at width {width}"
                );
            }
        }
    }
}
