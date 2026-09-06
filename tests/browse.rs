//! `browse`: the catalog as a checklist. Under a pipe it prints the objects
//! `catalog --json` prints; at a pseudo-terminal without fzf the picker of
//! this crate's own saves what was ticked and shows its `ls` row. And the
//! provenance it relies on: `add --catalog` records the id, and refuses to
//! save the same entry twice under different names without `--force`.

mod common;

use common::{catalog_entries, mcpdial, run, start, temp_home, Mode};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

fn saved(home: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("servers.json")).unwrap()).unwrap()
}

fn at_fixture(home: &Path, fixture: &Path, base: &str) -> Command {
    let mut c = mcpdial(home);
    c.env("MCPDIAL_CATALOG", fixture)
        .env("MCPDIAL_REGISTRY", base);
    c
}

#[test]
fn piped_browse_prints_the_same_objects_as_catalog() {
    let s = start(Mode::Stateless);
    let home = temp_home("browse-piped");
    let fixture = home.join("fixture.json");
    std::fs::write(&fixture, catalog_entries(&s.base).to_string()).unwrap();
    let at = || at_fixture(&home, &fixture, &s.base);

    let expected = run(at().args(["catalog", "--json"]));
    assert_eq!(expected.code, 0, "{}", expected.stderr);
    let o = run(at().args(["browse", "--json"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, expected.stdout);
    assert_eq!(o.stderr, "");

    // A pipe without --json, and --plain, get the objects as well: only a
    // person at a terminal gets the list.
    let o = run(at().arg("browse"));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, expected.stdout);
    let o = run(at().args(["browse", "--plain"]));
    assert_eq!(o.stdout, expected.stdout);

    // --all swaps the catalog for the registry's index: its own objects.
    let o = run(at().args(["browse", "--all", "--json"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let printed: Vec<Value> = serde_json::from_str(&o.stdout).unwrap();
    let mut names: Vec<&str> = printed
        .iter()
        .map(|e| e["server"]["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "io.github.acme/box",
            "io.github.acme/files",
            "io.github.acme/legacy",
            "io.github.acme/remote"
        ]
    );

    // The preview pane's command needs no terminal, and says when there is
    // nothing written for it to show.
    let o = run(at().args(["browse", "--preview", "remote"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, "no preview for remote\n");
}

#[test]
fn add_from_the_catalog_records_the_id_and_refuses_a_second_name_for_it() {
    let s = start(Mode::Stateless);
    let home = temp_home("browse-provenance");
    let fixture = home.join("fixture.json");
    std::fs::write(&fixture, catalog_entries(&s.base).to_string()).unwrap();
    let at = || at_fixture(&home, &fixture, &s.base);

    let o = run(at().args(["add", "web", "--catalog", "remote", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(at().args(["add", "direct", "--catalog", "fake", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let file = saved(&home);
    assert_eq!(
        file["servers"]["web"]["source"],
        json!({"catalog": "remote", "registry": "io.github.acme/remote", "version": "2.0.0"}),
        "the catalog id joins the registry's own provenance"
    );
    assert_eq!(
        file["servers"]["direct"]["source"],
        json!({"catalog": "fake"}),
        "a config entry records the id alone"
    );

    // The same entry under another name is refused, naming where it is.
    let o = run(at().args(["add", "web2", "--catalog", "remote", "--no-probe"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("catalog entry remote is already saved as web; pass --force"),
        "{}",
        o.stderr
    );
    assert!(saved(&home)["servers"].get("web2").is_none());
    let o = run(at().args(["--json", "add", "web2", "--catalog", "remote", "--no-probe"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let e: Value = serde_json::from_str(o.stderr.trim()).unwrap();
    assert_eq!(e["error"]["kind"], "usage");

    // --force saves it again; the same name again is the ordinary duplicate.
    let o = run(at().args([
        "add",
        "web2",
        "--catalog",
        "remote",
        "--no-probe",
        "--force",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let o = run(at().args(["add", "web", "--catalog", "remote", "--no-probe"]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(o.stderr.contains("web is already saved"), "{}", o.stderr);

    // `ls` shows the id in the SOURCE column.
    let o = run(mcpdial(&home).args(["ls", "--no-probe"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let row = o
        .stdout
        .lines()
        .find(|l| l.starts_with("direct"))
        .unwrap_or_else(|| panic!("no row for direct:\n{}", o.stdout));
    assert_eq!(row.split_whitespace().nth(3), Some("fake"), "{row}");
}

/// The picker of our own, driven through a pseudo-terminal: `script` lends
/// one on macOS and Linux; Windows has no such tool, so this stays unix.
/// Without the `rich` feature there is no picker to drive, only the exit 2
/// that `browse` gives a terminal it cannot show a list on.
#[cfg(all(unix, feature = "rich"))]
#[test]
fn at_a_terminal_without_fzf_the_picker_saves_what_was_ticked_and_dials_it() {
    use common::echo_command;
    use std::io::{Read, Write};
    use std::process::Stdio;

    let home = temp_home("browse-pty");
    let fixture = home.join("fixture.json");
    // The echo server as the first entry, so the cursor starts on it, next to
    // one that stays unticked.
    let entries = json!([
        {"id": "echo", "name": "Echo", "category": "Source control",
         "summary": "The echo server", "config": {"stdio": echo_command()},
         "transport": "stdio", "auth": "none"},
        {"id": "other", "name": "Other", "category": "Local files",
         "summary": "Left alone", "config": {"http": "http://127.0.0.1:1/mcp"},
         "transport": "http", "auth": "none"}
    ]);
    std::fs::write(&fixture, entries.to_string()).unwrap();
    let no_fzf = home.join("empty-path");
    std::fs::create_dir_all(&no_fzf).unwrap();
    let exe = env!("CARGO_BIN_EXE_mcpdial");

    let mut script = Command::new("/usr/bin/script");
    if cfg!(target_os = "macos") {
        script.args(["-q", "/dev/null", exe, "browse"]);
    } else {
        script.args(["-q", "-e", "-c", &format!("'{exe}' browse"), "/dev/null"]);
    }
    let mut child = script
        .env("MCPDIAL_HOME", &home)
        .env("MCPDIAL_CATALOG", &fixture)
        .env("PATH", &no_fzf)
        .env("TERM", "xterm")
        .env_remove("MCPDIAL_PLAIN")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn script");
    // Space ticks the first row, Enter applies. The terminal holds the keys
    // until the picker reads them, and the pipe stays open until it is done
    // so the terminal never sees a hangup.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b" \r").unwrap();
    stdin.flush().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        stderr.read_to_string(&mut s).ok();
        s
    });
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).unwrap();
    let status = child.wait().unwrap();
    drop(stdin);
    let out = String::from_utf8_lossy(&out).into_owned();
    let err = errors.join().unwrap();
    assert!(status.success(), "script exited {status}\n{out}\n{err}");

    let file = saved(&home);
    assert_eq!(file["servers"]["echo"]["stdio"], echo_command(), "{out}");
    assert_eq!(
        file["servers"]["echo"]["source"],
        json!({"catalog": "echo"})
    );
    assert!(
        file["servers"].get("other").is_none(),
        "the unticked row stays out: {file}"
    );
    assert!(out.contains("saved echo (stdio "), "{out}");
    let row = out
        .lines()
        .find(|l| l.trim_start().starts_with("echo ") && l.contains("connected"))
        .unwrap_or_else(|| panic!("no ls row for echo:\n{out}"));
    assert!(row.contains("echo-server"), "{row}");
}
