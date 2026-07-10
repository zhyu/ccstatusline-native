use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OscTerminator {
    Bell,
    StringTerminator,
}

#[derive(Debug, Clone, Copy)]
struct Escape<'a> {
    text: &'a str,
    next: usize,
    osc8: Option<(bool, OscTerminator)>,
}

fn escape_at(input: &str, start: usize) -> Option<Escape<'_>> {
    let bytes = input.as_bytes();
    let first_character = input.get(start..)?.chars().next()?;
    if first_character != '\u{1b}' && first_character != '\u{9b}' && first_character != '\u{9d}' {
        return None;
    }

    let (kind, body_start) = if first_character == '\u{1b}' {
        match bytes.get(start + 1).copied() {
            Some(b'[') => (b'[', start + 2),
            Some(b']') => (b']', start + 2),
            Some(_) => {
                let next_character = input[start + 1..]
                    .chars()
                    .next()
                    .expect("byte exists at a UTF-8 boundary");
                let next = start + 1 + next_character.len_utf8();
                return Some(Escape {
                    text: &input[start..next],
                    next,
                    osc8: None,
                });
            }
            None => {
                return Some(Escape {
                    text: &input[start..],
                    next: input.len(),
                    osc8: None,
                });
            }
        }
    } else if first_character == '\u{9b}' {
        (b'[', start + first_character.len_utf8())
    } else {
        (b']', start + first_character.len_utf8())
    };

    if kind == b'[' {
        let mut index = body_start;
        while let Some(byte) = bytes.get(index).copied() {
            index += 1;
            if (0x40..=0x7e).contains(&byte) {
                break;
            }
        }
        return Some(Escape {
            text: &input[start..index.min(input.len())],
            next: index.min(input.len()),
            osc8: None,
        });
    }

    let mut index = body_start;
    let (end, terminator) = loop {
        let Some(character) = input[index..].chars().next() else {
            break (input.len(), None);
        };
        match character {
            '\u{7}' => break (index + 1, Some(OscTerminator::Bell)),
            '\u{9c}' => {
                break (
                    index + character.len_utf8(),
                    Some(OscTerminator::StringTerminator),
                );
            }
            '\u{1b}' if bytes.get(index + 1) == Some(&b'\\') => {
                break (index + 2, Some(OscTerminator::StringTerminator));
            }
            _ => index += character.len_utf8(),
        }
    };
    let body_end = match terminator {
        Some(OscTerminator::Bell) => end.saturating_sub(1),
        Some(OscTerminator::StringTerminator)
            if bytes.get(end.saturating_sub(2)) == Some(&0x1b) =>
        {
            end.saturating_sub(2)
        }
        Some(OscTerminator::StringTerminator) if input[..end].ends_with('\u{9c}') => {
            end.saturating_sub('\u{9c}'.len_utf8())
        }
        Some(OscTerminator::StringTerminator) => end.saturating_sub(1),
        None => end,
    };
    let body = &input[body_start..body_end];
    let osc8 = body.strip_prefix("8;").and_then(|rest| {
        rest.find(';').map(|separator| {
            let opening = !rest[separator + 1..].is_empty();
            (
                opening,
                terminator.unwrap_or(OscTerminator::StringTerminator),
            )
        })
    });
    Some(Escape {
        text: &input[start..end],
        next: end,
        osc8,
    })
}

pub fn visible_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if let Some(escape) = escape_at(input, index) {
            index = escape.next;
            continue;
        }
        let character = input[index..].chars().next().expect("valid UTF-8 boundary");
        output.push(character);
        index += character.len_utf8();
    }
    output
}

pub fn visible_width(input: &str) -> usize {
    visible_text(input)
        .graphemes(true)
        .map(UnicodeWidthStr::width)
        .sum()
}

/// Unicode-width engines disagree on a few composition classes. Delegate
/// those strings rather than silently choosing a different truncation point.
pub fn requires_reference_width(input: &str) -> bool {
    input.chars().any(|character| {
        let code = character as u32;
        character == '\u{200d}'
            || (0x1100..=0x11ff).contains(&code)
            || (0x3130..=0x318f).contains(&code)
            || (0xa960..=0xa97f).contains(&code)
            || (0xd7b0..=0xd7ff).contains(&code)
    })
}

pub fn truncate_styled(input: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if visible_width(input) <= max_width {
        return input.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let target = max_width - 3;
    let mut output = String::with_capacity(input.len());
    let mut current_width = 0;
    let mut index = 0;
    let mut open_osc8 = None;

    while index < input.len() {
        if let Some(escape) = escape_at(input, index) {
            output.push_str(escape.text);
            index = escape.next;
            if let Some((opening, terminator)) = escape.osc8 {
                open_osc8 = opening.then_some(terminator);
            }
            continue;
        }

        let next_escape = input[index..]
            .char_indices()
            .find_map(|(offset, _)| escape_at(input, index + offset).map(|_| index + offset))
            .unwrap_or(input.len());
        let segment = &input[index..next_escape];
        let cluster = segment
            .graphemes(true)
            .next()
            .expect("non-empty visible segment");
        let width = UnicodeWidthStr::width(cluster);
        if current_width + width > target {
            break;
        }
        output.push_str(cluster);
        current_width += width;
        index += cluster.len();
    }

    match open_osc8 {
        Some(OscTerminator::Bell) => output.push_str("\x1b]8;;\x07"),
        Some(OscTerminator::StringTerminator) => output.push_str("\x1b]8;;\x1b\\"),
        None => {}
    }
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_sequences_are_zero_width() {
        assert_eq!(visible_width("\x1b[38;2;1;2;3mhello\x1b[39m"), 5);
    }

    #[test]
    fn handles_common_terminal_clusters() {
        assert_eq!(visible_width("e\u{301}"), 1);
        assert_eq!(visible_width("⚠"), 1);
        assert_eq!(visible_width("⚠️"), 2);
        assert_eq!(visible_width("👨‍👩‍👧‍👦"), 2);
        assert_eq!(visible_width("🇯🇵"), 2);
        assert_eq!(visible_width(""), 1);
    }

    #[test]
    fn truncation_keeps_codes_and_matches_short_widths() {
        let styled = "\x1b[31mabcdef\x1b[39m";
        assert_eq!(truncate_styled(styled, 3), "...");
        assert_eq!(truncate_styled(styled, 5), "\x1b[31mab...");
    }

    #[test]
    fn escape_followed_by_non_ascii_never_slices_inside_utf8() {
        let text = "a\x1béb";
        assert_eq!(visible_text(text), "ab");
        assert_eq!(visible_width(text), 2);
    }

    #[test]
    fn recognizes_c1_csi_and_osc_characters() {
        assert_eq!(visible_text("\u{9b}31mred\u{9b}39m"), "red");
        assert_eq!(visible_text("\u{9d}0;title\u{9c}text"), "text");
    }

    #[test]
    fn delegates_width_classes_that_differ_from_string_width() {
        assert!(requires_reference_width("क्‍ष"));
        assert!(requires_reference_width("가"));
        assert!(!requires_reference_width("가"));
        assert!(!requires_reference_width("e\u{301}"));
    }
}
