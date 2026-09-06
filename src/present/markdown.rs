//! Markdown as a person reads it at a terminal, and the guess of whether a
//! server sent any.
//!
//! Most tool results are markdown, and at a terminal its asterisks and pipes
//! are noise. A block whose first lines hold a heading, a list item, a fenced
//! code block, a table row or a block quote goes through `termimad`; anything
//! else is printed exactly as it came, so a one-line answer is never touched.
//!
//! The wrong guess is cheap on purpose. `termimad` renders line by line rather
//! than reflowing paragraphs, so every line break a server sent survives and
//! only a line wider than the screen is wrapped; the most a misread block
//! loses is the `*` and `_` around a word, which `--raw` gets back.

use termimad::crossterm::style::Attribute;
use termimad::{CompoundStyle, MadSkin, StyledChar};

/// How far down to look for a marker. A document says what it is inside its
/// first screenful, and a lone `- ` a hundred lines into prose is a dash.
const LINES_READ: usize = 24;

/// What to wrap to where the terminal will not say how wide it is, which is a
/// Windows console and a `--color always` pipe.
const ASSUMED_WIDTH: usize = 80;

/// Whether `text` was written as markdown. Emphasis on its own does not count:
/// a sentence with a `*` in it is still a sentence.
pub fn looks_like(text: &str) -> bool {
    text.lines().take(LINES_READ).any(is_marker)
}

fn is_marker(line: &str) -> bool {
    let line = line.trim_start();
    heading(line) || fence(line) || bullet(line) || numbered(line) || table_row(line) || quote(line)
}

/// `#` through `######`, and then a space, as a heading is written.
fn heading(line: &str) -> bool {
    let hashes = line.len() - line.trim_start_matches('#').len();
    (1..=6).contains(&hashes) && line[hashes..].starts_with(' ')
}

fn fence(line: &str) -> bool {
    line.starts_with("```") || line.starts_with("~~~")
}

fn bullet(line: &str) -> bool {
    matches!(line.as_bytes(), [b'-' | b'*' | b'+', b' ', ..])
}

/// A number, then `.` or `)`, then a space.
fn numbered(line: &str) -> bool {
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let after = line.as_bytes();
    digits > 0
        && matches!(after.get(digits), Some(b'.' | b')'))
        && after.get(digits + 1) == Some(&b' ')
}

/// A row of a table, in the one shape termimad's parser reads: a leading pipe,
/// and a second one somewhere after it.
fn table_row(line: &str) -> bool {
    line.strip_prefix('|')
        .is_some_and(|rest| rest.contains('|'))
}

fn quote(line: &str) -> bool {
    line == ">" || line.starts_with("> ")
}

/// A quiet skin: bold headings, and code, bullets, quote bars and table borders
/// dimmed so the structure recedes behind the words. It is attributes alone, no
/// colours, both because that is the whole of what the plan asked for and
/// because every sequence it emits is one of the SGR handful the rest of
/// mcpdial paints with. Without colour it is `termimad`'s own styleless skin,
/// which leaves the layout as the only thing rendering adds.
pub fn skin(color: bool) -> MadSkin {
    let mut skin = MadSkin::no_style();
    if !color {
        return skin;
    }
    for header in &mut skin.headers {
        header.add_attr(Attribute::Bold);
    }
    skin.bold.add_attr(Attribute::Bold);
    skin.italic.add_attr(Attribute::Italic);
    skin.strikeout.add_attr(Attribute::CrossedOut);
    let dim = CompoundStyle::with_attr(Attribute::Dim);
    skin.inline_code = dim;
    skin.code_block = dim.into();
    skin.table = dim.into();
    skin.bullet = StyledChar::new(dim, skin.bullet.nude_char());
    skin.quote_mark = StyledChar::new(dim, skin.quote_mark.nude_char());
    skin.horizontal_rule = StyledChar::new(dim, skin.horizontal_rule.nude_char());
    skin
}

/// `text` rendered for a screen `width` columns wide, ending in the newline
/// its last line needs.
pub fn rendered(text: &str, skin: &MadSkin, width: Option<usize>) -> String {
    skin.text(text, Some(width.unwrap_or(ASSUMED_WIDTH)))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_in_the_first_lines_is_what_says_markdown() {
        for md in [
            "# Title",
            "###### deepest",
            "  ## indented under a list",
            "intro\n\n## Section\n\ntext",
            "```rust\nfn main() {}\n```",
            "~~~\ncode\n~~~",
            "- one\n- two",
            "* one",
            "+ one",
            "  - nested",
            "1. first\n2. second",
            "12) twelfth",
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "|-----|:--|",
            "> quoted",
            ">",
        ] {
            assert!(looks_like(md), "{md:?} is markdown");
        }
    }

    #[test]
    fn prose_a_server_wrote_is_left_alone() {
        for plain in [
            "",
            "Successfully navigated to https://example.com.",
            "The sum of 1 and 2 is 3.",
            "Error: k is invalid. Valid keys are: Enter, ShiftLeft",
            "a sentence with an * in it and a - dash",
            "#hashtag, not a heading",
            "#!/usr/bin/env bash",
            "5.0 is a number, not a list",
            "a | b is not a table row",
            "{\"ok\": true}",
            "-- a comment\n--another",
            ">redirect",
            // The marker is past the screenful a document introduces itself in.
            &format!("{}\n# late", "prose\n".repeat(LINES_READ)),
        ] {
            assert!(!looks_like(plain), "{plain:?} is not markdown");
        }
    }

    #[test]
    fn rendering_keeps_every_line_break_and_wraps_only_what_is_too_wide() {
        let skin = skin(false);
        assert_eq!(
            rendered("- one\n- two\n\nplain line\nsecond line\n", &skin, Some(40)),
            "• one\n• two\n\nplain line\nsecond line\n"
        );
        assert_eq!(
            rendered("# T\n0123456789 0123456789", &skin, Some(12)),
            "T\n0123456789 \n0123456789\n"
        );
    }

    #[test]
    fn a_heading_is_bold_only_with_colour_on() {
        assert_eq!(
            rendered("# Title\nplain", &skin(true), Some(40)),
            "\u{1b}[1mTitle\u{1b}[0m\nplain\n"
        );
        assert_eq!(
            rendered("# Title\nplain", &skin(false), Some(40)),
            "Title\nplain\n"
        );
    }
}
