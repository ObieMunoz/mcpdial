//! The handful of SGR sequences a terminal is painted with, and the one
//! decision of whether to paint at all. JSON uses part of it; the ls status
//! column and the error prefix (#74) are to use the rest.
#![allow(dead_code)]

/// Whether colour is wanted: `NO_COLOR` (<https://no-color.org>) set to
/// anything but the empty string turns it off.
pub fn wanted() -> bool {
    wanted_by(std::env::var_os("NO_COLOR").as_deref())
}

fn wanted_by(no_color: Option<&std::ffi::OsStr>) -> bool {
    no_color.is_none_or(|v| v.is_empty())
}

/// The standard foreground colours, numbered as SGR does from 30.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Red = 1,
    Green = 2,
    Yellow = 3,
    Blue = 4,
    Magenta = 5,
    Cyan = 6,
}

/// What one span of text is painted with. The plain style paints nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    bold: bool,
    dim: bool,
    color: Option<Color>,
}

impl Style {
    /// The sequence that turns every attribute off.
    pub const OFF: &'static str = "\x1b[0m";

    pub const fn new() -> Self {
        Self {
            bold: false,
            dim: false,
            color: None,
        }
    }

    pub const fn bold(self) -> Self {
        Self { bold: true, ..self }
    }

    pub const fn dim(self) -> Self {
        Self { dim: true, ..self }
    }

    pub const fn color(self, color: Color) -> Self {
        Self {
            color: Some(color),
            ..self
        }
    }

    /// The sequence that turns the style on; empty for the plain style.
    pub fn on(self) -> String {
        let mut codes: Vec<u8> = Vec::new();
        if self.bold {
            codes.push(1);
        }
        if self.dim {
            codes.push(2);
        }
        if let Some(color) = self.color {
            codes.push(30 + color as u8);
        }
        if codes.is_empty() {
            return String::new();
        }
        let codes: Vec<String> = codes.iter().map(u8::to_string).collect();
        format!("\x1b[{}m", codes.join(";"))
    }

    /// `text` in this style, and as given when the style is plain.
    pub fn paint(self, text: &str) -> String {
        let on = self.on();
        if on.is_empty() {
            text.to_string()
        } else {
            format!("{on}{text}{}", Self::OFF)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn no_color_set_to_anything_but_nothing_turns_colour_off() {
        assert!(wanted_by(None));
        assert!(wanted_by(Some(OsStr::new(""))));
        assert!(!wanted_by(Some(OsStr::new("1"))));
        assert!(!wanted_by(Some(OsStr::new("0"))));
    }

    #[test]
    fn a_style_is_one_sgr_sequence_and_the_plain_style_is_none() {
        assert_eq!(Style::new().on(), "");
        assert_eq!(Style::new().paint("x"), "x");
        assert_eq!(Style::new().bold().on(), "\x1b[1m");
        assert_eq!(Style::new().dim().color(Color::Red).on(), "\x1b[2;31m");
        assert_eq!(
            Style::new().bold().color(Color::Green).paint("ok"),
            "\x1b[1;32mok\x1b[0m"
        );
        assert_eq!(Style::new().color(Color::Cyan).on(), "\x1b[36m");
    }
}
