//! JSON as a person reads it at a terminal: serde_json's own pretty printing,
//! with keys, strings, numbers, booleans and null each in a colour of its own.

use super::style::{Color, Style};
use serde::Serialize;
use serde_json::ser::{Formatter, PrettyFormatter};
use serde_json::Value;
use std::io::{self, Write};

const KEY: Style = Style::new().bold().color(Color::Blue);
const STRING: Style = Style::new().color(Color::Green);
const NUMBER: Style = Style::new().color(Color::Cyan);
const BOOL: Style = Style::new().color(Color::Yellow);
const NULL: Style = Style::new().color(Color::Magenta);

/// `text` in colour when it is an object or an array laid out exactly as
/// `serde_json::to_string_pretty` lays one out, and `None` for anything else.
/// The layout stays serde_json's, so the text is identical with the colour
/// stripped.
pub fn highlighted(text: &str) -> Option<String> {
    if !text.starts_with(['{', '[']) {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    if serde_json::to_string_pretty(&value).ok()? != text {
        return None;
    }
    Some(paint(&value))
}

pub fn paint(value: &Value) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, Painter::default());
    value
        .serialize(&mut ser)
        .expect("a Value serializes to a Vec");
    String::from_utf8(out).expect("serde_json writes UTF-8")
}

/// serde_json's pretty formatter with every scalar wrapped in its colour. The
/// layout is the inner formatter's; this one only knows whether the string it
/// is in is a key.
#[derive(Default)]
struct Painter<'a> {
    layout: PrettyFormatter<'a>,
    in_key: bool,
}

fn painted<W: ?Sized + Write>(
    w: &mut W,
    style: Style,
    write: impl FnOnce(&mut W) -> io::Result<()>,
) -> io::Result<()> {
    w.write_all(style.on().as_bytes())?;
    write(w)?;
    w.write_all(Style::OFF.as_bytes())
}

impl Formatter for Painter<'_> {
    fn write_null<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(NULL.paint("null").as_bytes())
    }

    fn write_bool<W: ?Sized + Write>(&mut self, w: &mut W, value: bool) -> io::Result<()> {
        painted(w, BOOL, |w| self.layout.write_bool(w, value))
    }

    fn write_i64<W: ?Sized + Write>(&mut self, w: &mut W, value: i64) -> io::Result<()> {
        painted(w, NUMBER, |w| self.layout.write_i64(w, value))
    }

    fn write_u64<W: ?Sized + Write>(&mut self, w: &mut W, value: u64) -> io::Result<()> {
        painted(w, NUMBER, |w| self.layout.write_u64(w, value))
    }

    fn write_f64<W: ?Sized + Write>(&mut self, w: &mut W, value: f64) -> io::Result<()> {
        painted(w, NUMBER, |w| self.layout.write_f64(w, value))
    }

    fn begin_string<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        let style = if self.in_key { KEY } else { STRING };
        w.write_all(style.on().as_bytes())?;
        self.layout.begin_string(w)
    }

    fn end_string<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.layout.end_string(w)?;
        w.write_all(Style::OFF.as_bytes())
    }

    fn begin_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.layout.begin_array(w)
    }

    fn end_array<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.layout.end_array(w)
    }

    fn begin_array_value<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.layout.begin_array_value(w, first)
    }

    fn end_array_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.layout.end_array_value(w)
    }

    fn begin_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.layout.begin_object(w)
    }

    fn end_object<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.layout.end_object(w)
    }

    fn begin_object_key<W: ?Sized + Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        self.in_key = true;
        self.layout.begin_object_key(w, first)
    }

    fn end_object_key<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.in_key = false;
        self.layout.end_object_key(w)
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.layout.begin_object_value(w)
    }

    fn end_object_value<W: ?Sized + Write>(&mut self, w: &mut W) -> io::Result<()> {
        self.layout.end_object_value(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `text` without its SGR sequences.
    fn stripped(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("\x1b[") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after.find('m').expect("an SGR sequence ends in m");
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }

    fn representative() -> Value {
        json!({
            "a": [1, -2, 3.5],
            "b": {"c": "x\"y\n\u{1b}", "d": true, "e": false, "f": null},
            "g": [],
            "h": {}
        })
    }

    #[test]
    fn stripped_of_colour_the_text_is_to_string_pretty() {
        let value = representative();
        assert_eq!(
            stripped(&paint(&value)),
            serde_json::to_string_pretty(&value).unwrap()
        );
    }

    #[test]
    fn every_kind_of_token_has_its_own_colour() {
        assert_eq!(
            paint(&representative()),
            concat!(
                "{\n",
                "  \x1b[1;34m\"a\"\x1b[0m: [\n",
                "    \x1b[36m1\x1b[0m,\n",
                "    \x1b[36m-2\x1b[0m,\n",
                "    \x1b[36m3.5\x1b[0m\n",
                "  ],\n",
                "  \x1b[1;34m\"b\"\x1b[0m: {\n",
                "    \x1b[1;34m\"c\"\x1b[0m: \x1b[32m\"x\\\"y\\n\\u001b\"\x1b[0m,\n",
                "    \x1b[1;34m\"d\"\x1b[0m: \x1b[33mtrue\x1b[0m,\n",
                "    \x1b[1;34m\"e\"\x1b[0m: \x1b[33mfalse\x1b[0m,\n",
                "    \x1b[1;34m\"f\"\x1b[0m: \x1b[35mnull\x1b[0m\n",
                "  },\n",
                "  \x1b[1;34m\"g\"\x1b[0m: [],\n",
                "  \x1b[1;34m\"h\"\x1b[0m: {}\n",
                "}"
            )
        );
    }

    #[test]
    fn only_a_pretty_printed_document_is_highlighted() {
        let value = representative();
        let pretty = serde_json::to_string_pretty(&value).unwrap();
        assert_eq!(highlighted(&pretty), Some(paint(&value)));
        assert_eq!(highlighted(&value.to_string()), None);
        assert_eq!(highlighted(&pretty.replace("  ", "    ")), None);
        assert_eq!(highlighted("3"), None);
        assert_eq!(highlighted("\"text\""), None);
        assert_eq!(highlighted("{not json"), None);
        assert_eq!(highlighted("Successfully navigated."), None);
    }
}
