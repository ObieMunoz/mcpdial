//! The handful of SGR sequences a terminal is painted with, and the one
//! decision of whether to paint at all: `--color`, `NO_COLOR` and whether the
//! stream is a terminal. Everything `Rich` colours comes through here — the
//! JSON of #79, the ls status column and the error prefix of #74.

/// `--color`: when to send SGR sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    /// Even into a pipe or a file, for `less -R`
    Always,
    Never,
    /// At a terminal, unless NO_COLOR is set
    Auto,
}

/// Whether colour goes to a stream: `always` and `never` decide alone, `auto`
/// wants a terminal and no `NO_COLOR` (<https://no-color.org>) set to anything
/// but the empty string. Which streams are terminals is `Presenter::choose`'s
/// to find out; this only weighs what it saw.
#[cfg(feature = "rich")]
pub fn color_enabled(mode: ColorMode, is_terminal: bool) -> bool {
    decide(mode, is_terminal, std::env::var_os("NO_COLOR").as_deref())
}

#[cfg(feature = "rich")]
fn decide(mode: ColorMode, is_terminal: bool, no_color: Option<&std::ffi::OsStr>) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => is_terminal && no_color.is_none_or(|v| v.is_empty()),
    }
}

/// The standard foreground colours, numbered as SGR does from 30.
#[cfg(feature = "rich")]
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
#[cfg(feature = "rich")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    bold: bool,
    dim: bool,
    color: Option<Color>,
}

#[cfg(feature = "rich")]
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

    /// This style where colour goes to the stream, and the plain style, which
    /// paints nothing, where it does not.
    pub const fn when(self, on: bool) -> Self {
        if on {
            self
        } else {
            Self::new()
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

#[cfg(all(test, feature = "rich"))]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn auto_wants_a_terminal_and_no_color_set_to_anything_but_nothing() {
        assert!(decide(ColorMode::Auto, true, None));
        assert!(decide(ColorMode::Auto, true, Some(OsStr::new(""))));
        assert!(!decide(ColorMode::Auto, true, Some(OsStr::new("1"))));
        assert!(!decide(ColorMode::Auto, true, Some(OsStr::new("0"))));
        assert!(!decide(ColorMode::Auto, false, None));
    }

    #[test]
    fn always_and_never_ignore_the_environment_and_the_stream() {
        assert!(decide(ColorMode::Always, false, Some(OsStr::new("1"))));
        assert!(!decide(ColorMode::Never, true, None));
    }

    #[test]
    fn a_style_is_one_sgr_sequence_and_the_plain_style_is_none() {
        assert_eq!(Style::new().on(), "");
        assert_eq!(Style::new().paint("x"), "x");
        assert_eq!(Style::new().bold().on(), "\x1b[1m");
        assert_eq!(Style::new().color(Color::Red).on(), "\x1b[31m");
        assert_eq!(
            Style::new().bold().color(Color::Green).paint("ok"),
            "\x1b[1;32mok\x1b[0m"
        );
        assert_eq!(Style::new().color(Color::Cyan).on(), "\x1b[36m");
    }

    #[test]
    fn a_style_turned_off_paints_nothing() {
        let red = Style::new().bold().color(Color::Red);
        assert_eq!(red.when(true).paint("error:"), "\x1b[1;31merror:\x1b[0m");
        assert_eq!(red.when(false).paint("error:"), "error:");
    }
}
