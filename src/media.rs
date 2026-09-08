//! Image, audio and blob blocks on their way out of a result.
//!
//! A server may answer with bytes, and a terminal is the wrong place for them.
//! `--save-dir` turns every such block into a file named after what produced it
//! and leaves a line saying so; without it the block is described rather than
//! printed. Either way the text around it is rendered exactly as it always was.

use crate::output::{As, Output, Payload};
use crate::present::Presenter;
use crate::{output, print_value, shape, Failure};
use mcpdial::session::{
    extension_for, render_resource, resource_bodies, save_media, Media, MediaSink,
};
use mcpdial::Error;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Where a media block's bytes go: a numbered file under `--save-dir`, or nowhere,
/// leaving a placeholder on stdout.
pub(crate) struct MediaFiles<'a> {
    pub(crate) dir: Option<&'a Path>,
    /// The tool, prompt or resource the bytes came from, as a file name.
    pub(crate) stem: String,
}

impl MediaFiles<'_> {
    fn saves(&self) -> bool {
        self.dir.is_some()
    }

    /// The first free `<stem>-<n>.<ext>` in the directory, so a second call keeps
    /// the first one's file rather than writing over it.
    fn place(&self, media: &Media) -> Result<Option<PathBuf>, Error> {
        let Some(dir) = self.dir else {
            return Ok(None);
        };
        let ext = extension_for(&media.mime_type);
        let failed =
            |path: &Path, e: std::io::Error| Error::transport(format!("{}: {e}", path.display()));
        for n in 1.. {
            let path = dir.join(format!("{}-{n}.{ext}", self.stem));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(&media.bytes).map_err(|e| failed(&path, e))?;
                    return Ok(Some(path));
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(failed(&path, e)),
            }
        }
        unreachable!("the counter does not run out")
    }
}

/// The sink of a reader with nowhere to put bytes.
pub(crate) fn placeholders(_: &Media) -> Result<Option<PathBuf>, Error> {
    Ok(None)
}

/// A media block's bytes filed where `--save-dir` says, and offered to the
/// presenter, which draws them at a terminal that can show an image and does
/// nothing anywhere else.
pub(crate) fn filed(
    ui: &dyn Presenter,
    files: &MediaFiles,
    media: &Media,
) -> Result<Option<PathBuf>, Error> {
    let path = files.place(media)?;
    ui.draw(media, path.as_deref());
    Ok(path)
}

/// A tool or prompt name as a file name: anything a shell or a filesystem would
/// argue with becomes `_`.
pub(crate) fn file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.trim_matches('.').is_empty() {
        "media".to_string()
    } else {
        stem
    }
}

/// A resource's file name from its URI: the last path segment, less its extension.
pub(crate) fn resource_stem(uri: &str) -> String {
    let last = uri
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let stem = match last.rsplit_once('.') {
        Some((before, _)) if !before.is_empty() => before,
        _ => last,
    };
    if stem.is_empty() {
        "resource".to_string()
    } else {
        file_stem(stem)
    }
}

pub(crate) type Render = fn(&Value, &mut MediaSink) -> Result<String, Error>;

/// The text `render` makes of a `tools/call` or `prompts/get` result, with its
/// media handed to `files`. Under `--json` the object itself is what gets printed,
/// so the media moves into it as `path` and `bytes` instead, and the text is only
/// for `call`'s argument-error check.
pub(crate) fn rendered(
    ui: &dyn Presenter,
    result: &mut Value,
    json: bool,
    files: &MediaFiles,
    render: Render,
) -> Result<String, Failure> {
    let mut sink = |m: &Media| filed(ui, files, m);
    if json {
        if files.saves() {
            save_media(result, &mut sink)?;
        }
        // The object goes out as the server sent it; a blob it mangled is the
        // reader's problem, not a reason to fail the command.
        return Ok(render(result, &mut placeholders).unwrap_or_default());
    }
    Ok(render(result, &mut sink)?)
}

/// A `prompts/get` result on stdout: the object under `--json`, the messages
/// otherwise.
pub(crate) fn emit(
    ui: &dyn Presenter,
    out: &Output,
    result: &mut Value,
    json: bool,
    compact: bool,
    files: &MediaFiles,
    render: Render,
) -> Result<(), Failure> {
    let text = rendered(ui, result, json, files, render)?;
    emit_rendered(ui, out, result, &text, json, compact)
}

/// The tail of [`emit`]: a result and the text already made of it, on stdout.
/// `show` takes this way too, with the text the result was first printed as,
/// so that reprinting one costs neither a request nor a second file of media.
pub(crate) fn emit_rendered(
    ui: &dyn Presenter,
    out: &Output,
    result: &Value,
    text: &str,
    json: bool,
    compact: bool,
) -> Result<(), Failure> {
    let payload = if json {
        Payload::Json {
            value: result,
            one_line: compact,
        }
    } else {
        Payload::Text(text)
    };
    let sent = out.deliver(payload, false)?;
    output::show(ui, sent, shape(json), json, || {
        if json {
            print_value(ui, result, compact);
        } else if !text.is_empty() {
            ui.text(text);
        }
    });
    Ok(())
}

/// A `resources/read` result on stdout: the object under `--json`; with a
/// `--save-dir`, its text and a line per blob filed there; otherwise the bytes
/// themselves, which is the presenter's business.
pub(crate) fn emit_resource(
    ui: &dyn Presenter,
    out: &Output,
    result: &mut Value,
    json: bool,
    compact: bool,
    files: &MediaFiles,
    redirect: &str,
) -> Result<(), Failure> {
    let mut sink = |m: &Media| filed(ui, files, m);
    if json {
        if files.saves() {
            save_media(result, &mut sink)?;
        }
        let sent = out.deliver(
            Payload::Json {
                value: result,
                one_line: compact,
            },
            false,
        )?;
        output::show(ui, sent, As::Json, json, || {
            print_value(ui, result, compact)
        });
    } else if files.saves() {
        let text = render_resource(result, &mut sink)?;
        let sent = out.deliver(Payload::Text(&text), false)?;
        output::show(ui, sent, As::Raw, json, || ui.out(&text));
    } else {
        // Bodies leave byte for byte, so the presenter that refuses binary at a
        // terminal keeps its say - unless `--output` has somewhere to put them.
        let bodies = resource_bodies(result)?;
        let sent = out.deliver_resource(&bodies)?;
        let mut refused = Ok(());
        output::show(ui, sent, As::Raw, json, || {
            refused = ui.resource(&bodies, redirect);
        });
        refused?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcpdial::session::Media;

    #[test]
    fn media_files_are_named_after_their_source_and_never_overwritten() {
        assert_eq!(file_stem("take_screenshot"), "take_screenshot");
        assert_eq!(file_stem("shot/one:two"), "shot_one_two");
        assert_eq!(file_stem(".."), "media");
        assert_eq!(resource_stem("file:///logo.png"), "logo");
        assert_eq!(resource_stem("file:///dir/a.b.c/"), "a.b");
        assert_eq!(resource_stem("https://host/x?y=1#z"), "x");
        assert_eq!(resource_stem("file:///.hidden"), ".hidden");
        assert_eq!(resource_stem(""), "resource");

        let dir = std::env::temp_dir().join(format!(
            "mcpdial-media-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let png = Media {
            kind: "image".into(),
            mime_type: "image/png".into(),
            bytes: b"\x89PNG".to_vec(),
        };
        let files = MediaFiles {
            dir: Some(&dir),
            stem: "shot".into(),
        };
        assert_eq!(files.place(&png).unwrap(), Some(dir.join("shot-1.png")));
        assert_eq!(files.place(&png).unwrap(), Some(dir.join("shot-2.png")));
        assert_eq!(std::fs::read(dir.join("shot-1.png")).unwrap(), b"\x89PNG");
        let nowhere = MediaFiles {
            dir: None,
            stem: "shot".into(),
        };
        assert_eq!(nowhere.place(&png).unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
