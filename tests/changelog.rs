use std::fs;
use std::path::Path;

fn changelog() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("CHANGELOG.md");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn section<'a>(text: &'a str, heading: &str) -> Option<(&'a str, Vec<&'a str>)> {
    let mut lines = text.lines().skip_while(|l| !l.starts_with(heading));
    let first = lines.next()?;
    let body = lines.take_while(|l| !l.starts_with("## ")).collect();
    Some((first, body))
}

fn is_date(s: &str) -> bool {
    s.len() == 10
        && s.char_indices().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        })
}

#[test]
fn the_current_version_has_release_notes() {
    let text = changelog();
    let version = env!("CARGO_PKG_VERSION");
    let heading = format!("## [{version}] - ");
    let (first, body) = section(&text, &heading)
        .unwrap_or_else(|| panic!("CHANGELOG.md has no `## [{version}] - YYYY-MM-DD` section"));

    let date = &first[heading.len()..];
    assert!(
        is_date(date),
        "the {version} heading ends in {date:?}, not a YYYY-MM-DD date"
    );
    assert!(
        body.iter().any(|l| l.starts_with("- ")),
        "the {version} section lists no changes"
    );
}

#[test]
fn unreleased_section_is_ready_for_the_next_change() {
    assert!(changelog().lines().any(|l| l == "## [Unreleased]"));
}
