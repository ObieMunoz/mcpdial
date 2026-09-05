use std::fs;
use std::path::Path;

fn changelog() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("CHANGELOG.md");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn section<'a>(text: &'a str, heading: &str) -> Option<&'a str> {
    let start = text.find(heading)?;
    let body = &text[start + heading.len()..];
    let end = body.find("\n## ").unwrap_or(body.len());
    Some(&body[..end])
}

#[test]
fn the_current_version_has_release_notes() {
    let text = changelog();
    let version = env!("CARGO_PKG_VERSION");
    let heading = format!("\n## [{version}] - ");
    let body = section(&text, &heading)
        .unwrap_or_else(|| panic!("CHANGELOG.md has no `## [{version}] - YYYY-MM-DD` section"));

    let date = body.lines().next().unwrap_or("");
    let dated = date.len() == 10
        && date.char_indices().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        });
    assert!(
        dated,
        "the {version} heading ends in {date:?}, not a YYYY-MM-DD date"
    );
    assert!(
        body.lines().any(|l| l.starts_with("- ")),
        "the {version} section lists no changes"
    );
}

#[test]
fn unreleased_section_is_ready_for_the_next_change() {
    assert!(changelog().contains("\n## [Unreleased]\n"));
}
