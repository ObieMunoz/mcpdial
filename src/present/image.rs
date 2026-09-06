//! Drawing an image on the terminals that can show one.
//!
//! Two escape sequences, hand-rolled around the bytes the server already sent:
//! iTerm2's OSC 1337, which WezTerm speaks too, and kitty's APC `_G`, which
//! Ghostty speaks too. Both take a PNG as it is, so nothing here decodes an
//! image, and anything that is not a PNG keeps the placeholder line of #63.
//!
//! Nothing here asks whether stdout is a terminal; `Presenter::choose` has
//! settled that already and only reaches this module when the answer was yes.
//! What is left is the narrower question of *which* terminal, and it is
//! answered conservatively: an escape sequence sent somewhere it is not
//! understood spills base64 across a person's screen, so an environment that
//! does not name itself gets the placeholder and nothing else.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

/// The eight bytes every PNG starts with.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// kitty takes the base64 in chunks of at most this many characters.
const KITTY_CHUNK: usize = 4096;

/// Which escape sequence draws an image on this terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// iTerm2's OSC 1337, which WezTerm speaks as well.
    Iterm2,
    /// kitty's APC `_G`, which Ghostty speaks as well.
    Kitty,
}

/// The protocol a terminal names itself by in the environment, and `None` for
/// one that names nothing recognisable. A multiplexer is `None` whatever the
/// terminal under it says: its passthrough is off by default and is not
/// something to guess at.
pub fn protocol(env: impl Fn(&str) -> Option<String>) -> Option<Protocol> {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    let term = var("TERM").unwrap_or_default();
    if var("TMUX").is_some() || term.starts_with("screen") || term.starts_with("tmux") {
        return None;
    }
    match var("TERM_PROGRAM").as_deref() {
        Some("iTerm.app") | Some("WezTerm") => return Some(Protocol::Iterm2),
        Some("ghostty") => return Some(Protocol::Kitty),
        _ => {}
    }
    let kitty =
        var("KITTY_WINDOW_ID").is_some() || term == "xterm-kitty" || term == "xterm-ghostty";
    kitty.then_some(Protocol::Kitty)
}

/// The escape sequence that draws `bytes` on a `protocol` terminal `cols` wide
/// whose every cell is `cell` pixels, and `None` for bytes that are not a PNG,
/// which is the only thing either sequence here can carry.
pub fn drawing(
    protocol: Protocol,
    bytes: &[u8],
    cols: Option<usize>,
    cell: Option<(usize, usize)>,
) -> Option<String> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return None;
    }
    let encoded = STANDARD.encode(bytes);
    Some(match protocol {
        Protocol::Iterm2 => iterm2(&encoded, bytes.len(), cols),
        Protocol::Kitty => kitty(&encoded, fitted(bytes, cols, cell)),
    })
}

/// iTerm2 fits an image inside the box it is given, so the width cap is the
/// box: as wide as the screen and as tall as the image already is, which can
/// only ever shrink it.
fn iterm2(encoded: &str, bytes: usize, cols: Option<usize>) -> String {
    let mut args = format!("inline=1;size={bytes}");
    if let Some(cols) = cols.filter(|cols| *cols > 0) {
        args.push_str(&format!(";width={cols};height=auto;preserveAspectRatio=1"));
    }
    format!("\x1b]1337;File={args}:{encoded}\x07")
}

/// kitty scales an image to whatever box it is given, so it is given one only
/// where the image is too wide for the screen. `a=T` is "transmit and display",
/// `f=100` a PNG the terminal decodes itself, and `m=1` says a chunk follows.
fn kitty(encoded: &str, cells: Option<(usize, usize)>) -> String {
    let mut opening = "a=T,f=100".to_string();
    if let Some((cols, rows)) = cells {
        opening.push_str(&format!(",c={cols},r={rows}"));
    }
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(KITTY_CHUNK)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64 is ASCII"))
        .collect();
    let mut out = String::with_capacity(encoded.len() + 64 * chunks.len());
    for (i, chunk) in chunks.iter().enumerate() {
        let more = usize::from(i + 1 < chunks.len());
        let control = match i {
            0 => format!("{opening},m={more}"),
            _ => format!("m={more}"),
        };
        out.push_str(&format!("\x1b_G{control};{chunk}\x1b\\"));
    }
    out
}

/// The cells a PNG wider than the screen should be drawn over: the full width,
/// and the height its own proportions ask for. `None` leaves it at its natural
/// size, which is also all that can be done for a terminal that will not say
/// how many pixels a cell takes.
fn fitted(png: &[u8], cols: Option<usize>, cell: Option<(usize, usize)>) -> Option<(usize, usize)> {
    let (cols, (cell_width, cell_height)) = (cols?, cell?);
    let (width, height) = png_size(png)?;
    let (width, height) = (width as usize, height as usize);
    let screen = cols.checked_mul(cell_width)?;
    if width == 0 || height == 0 || cell_height == 0 || width <= screen {
        return None;
    }
    let rows = (height * screen / width).div_ceil(cell_height).max(1);
    Some((cols, rows))
}

/// The pixel size in a PNG's header: the eight-byte signature, the length of
/// the first chunk, its name, and then IHDR's own first two fields. Reading two
/// numbers out of a header is not decoding the image, which nothing here does.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.get(12..16)? != b"IHDR" {
        return None;
    }
    let big_endian = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(png.get(at..at + 4)?.try_into().ok()?))
    };
    Some((big_endian(16)?, big_endian(20)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG header claiming `width` by `height`, with nothing after it: no
    /// pixel is ever looked at.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn each_terminal_is_known_by_what_it_puts_in_the_environment() {
        let found = |vars: &[(&str, &str)]| protocol(env_of(vars));
        assert_eq!(
            found(&[("TERM_PROGRAM", "iTerm.app")]),
            Some(Protocol::Iterm2)
        );
        assert_eq!(
            found(&[("TERM_PROGRAM", "WezTerm")]),
            Some(Protocol::Iterm2)
        );
        assert_eq!(found(&[("TERM_PROGRAM", "ghostty")]), Some(Protocol::Kitty));
        assert_eq!(found(&[("KITTY_WINDOW_ID", "1")]), Some(Protocol::Kitty));
        assert_eq!(found(&[("TERM", "xterm-kitty")]), Some(Protocol::Kitty));
        assert_eq!(found(&[("TERM", "xterm-ghostty")]), Some(Protocol::Kitty));
    }

    #[test]
    fn a_terminal_that_names_nothing_recognisable_draws_nothing() {
        let found = |vars: &[(&str, &str)]| protocol(env_of(vars));
        assert_eq!(found(&[]), None);
        assert_eq!(found(&[("TERM", "xterm-256color")]), None);
        assert_eq!(found(&[("TERM_PROGRAM", "Apple_Terminal")]), None);
        assert_eq!(found(&[("TERM_PROGRAM", "vscode")]), None);
        // Set but empty is unset.
        assert_eq!(
            found(&[("KITTY_WINDOW_ID", ""), ("TERM_PROGRAM", "")]),
            None
        );
    }

    #[test]
    fn a_multiplexer_draws_nothing_whatever_runs_under_it() {
        let found = |vars: &[(&str, &str)]| protocol(env_of(vars));
        assert_eq!(
            found(&[
                ("TMUX", "/tmp/tmux-501/default,1,0"),
                ("TERM_PROGRAM", "iTerm.app")
            ]),
            None
        );
        assert_eq!(found(&[("TERM", "screen"), ("KITTY_WINDOW_ID", "1")]), None);
        assert_eq!(found(&[("TERM", "screen.xterm-256color")]), None);
        assert_eq!(found(&[("TERM", "tmux-256color")]), None);
    }

    #[test]
    fn the_iterm2_sequence_wraps_the_base64_in_osc_1337() {
        let bytes = png(10, 10);
        let encoded = STANDARD.encode(&bytes);
        assert_eq!(
            drawing(Protocol::Iterm2, &bytes, None, None).unwrap(),
            format!("\x1b]1337;File=inline=1;size={}:{encoded}\x07", bytes.len())
        );
        assert_eq!(
            drawing(Protocol::Iterm2, &bytes, Some(80), None).unwrap(),
            format!(
                "\x1b]1337;File=inline=1;size={};width=80;height=auto;preserveAspectRatio=1:{encoded}\x07",
                bytes.len()
            )
        );
    }

    #[test]
    fn the_kitty_sequence_chunks_the_base64_and_marks_the_last_chunk() {
        let small = png(10, 10);
        let encoded = STANDARD.encode(&small);
        assert!(encoded.len() < KITTY_CHUNK);
        assert_eq!(
            drawing(Protocol::Kitty, &small, None, None).unwrap(),
            format!("\x1b_Ga=T,f=100,m=0;{encoded}\x1b\\")
        );

        let mut big = png(10, 10);
        big.resize(8192, 0);
        let encoded = STANDARD.encode(&big);
        let drawn = drawing(Protocol::Kitty, &big, None, None).unwrap();
        let chunks: Vec<&str> = drawn.split("\x1b\\").filter(|s| !s.is_empty()).collect();
        assert_eq!(chunks.len(), encoded.len().div_ceil(KITTY_CHUNK));
        assert!(chunks[0].starts_with("\x1b_Ga=T,f=100,m=1;"));
        assert!(chunks[1].starts_with("\x1b_Gm=1;"));
        assert!(chunks[chunks.len() - 1].starts_with("\x1b_Gm=0;"));
        let carried: String = chunks
            .iter()
            .map(|c| c.split_once(';').unwrap().1)
            .collect();
        assert_eq!(carried, encoded);
    }

    #[test]
    fn only_a_png_is_drawn() {
        for bytes in [b"\xff\xd8\xff\xe0jpeg".to_vec(), b"GIF89a".to_vec(), vec![]] {
            assert_eq!(drawing(Protocol::Iterm2, &bytes, Some(80), None), None);
            assert_eq!(drawing(Protocol::Kitty, &bytes, Some(80), None), None);
        }
    }

    #[test]
    fn a_png_header_gives_up_its_size_and_a_truncated_one_does_not() {
        assert_eq!(png_size(&png(1920, 1080)), Some((1920, 1080)));
        assert_eq!(png_size(PNG_SIGNATURE), None);
        assert_eq!(png_size(&png(1, 1)[..20]), None);
    }

    #[test]
    fn kitty_gets_a_box_only_for_an_image_too_wide_for_the_screen() {
        let cell = Some((10, 20));
        // 1600 pixels of screen, and an image that fits in them.
        assert_eq!(fitted(&png(800, 400), Some(160), cell), None);
        assert_eq!(fitted(&png(1600, 400), Some(160), cell), None);
        // Twice too wide: half the height, in cells of twenty pixels.
        assert_eq!(fitted(&png(3200, 800), Some(160), cell), Some((160, 20)));
        // A terminal that will not say how big a cell is caps nothing.
        assert_eq!(fitted(&png(3200, 800), Some(160), None), None);
        assert_eq!(fitted(&png(3200, 800), None, cell), None);
        // A header too short to measure, and a degenerate one.
        assert_eq!(fitted(PNG_SIGNATURE, Some(160), cell), None);
        assert_eq!(fitted(&png(3200, 0), Some(160), cell), None);
    }
}
