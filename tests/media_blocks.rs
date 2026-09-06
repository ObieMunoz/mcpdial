//! Image, audio and blob content blocks: a placeholder on stdout, a file under
//! `--save-dir`, and the server's own object under `--json`.

mod common;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use common::{echo_command, mcpdial, run, start, temp_home, Mode, PNG_MAGIC};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// What the echo server's `shot` tool and `poster` prompt carry: a PNG signature
/// padded to 4096 bytes.
fn shot_png() -> Vec<u8> {
    let mut png = PNG_MAGIC.to_vec();
    png.resize(4096, 0);
    png
}

fn echo_target() -> String {
    format!("stdio:{}", echo_command())
}

/// The binary, with the echo server's `poster` prompt switched on in the process
/// it spawns.
fn echo(home: &Path) -> Command {
    let mut c = mcpdial(home);
    c.env("ECHO_SERVER_PROMPTS", "1");
    c
}

#[test]
fn an_image_block_prints_as_a_placeholder_and_never_as_base64() {
    let home = temp_home("media-placeholder");
    let o = run(echo(&home).args(["call", &echo_target(), "shot"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, "done\n[image image/png, 4 KB]\n");
    assert!(
        !o.stdout.contains("iVBOR") && !o.stderr.contains("iVBOR"),
        "no base64 anywhere: {}",
        o.stdout
    );

    let o = run(echo(&home).args(["prompt", &echo_target(), "poster"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        o.stdout,
        "user: Caption this.\nuser: [image image/png, 4 KB]\n"
    );
}

#[test]
fn save_dir_writes_the_bytes_and_names_the_file() {
    let home = temp_home("media-save");
    let dir = home.join("shots");
    let o = run(echo(&home).args([
        "call",
        &echo_target(),
        "shot",
        "--save-dir",
        dir.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let first = dir.join("shot-1.png");
    assert_eq!(
        o.stdout,
        format!("done\n[image saved to {}, 4 KB]\n", first.display())
    );
    assert_eq!(
        std::fs::read(&first).unwrap(),
        shot_png(),
        "decoded, not base64"
    );

    // A second call keeps the first call's file.
    let o = run(echo(&home).args([
        "--save-dir",
        dir.to_str().unwrap(),
        "call",
        &echo_target(),
        "shot",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout
            .contains(&dir.join("shot-2.png").display().to_string()),
        "{}",
        o.stdout
    );
    assert_eq!(std::fs::read(&first).unwrap(), shot_png(), "still there");

    // A prompt's blocks are filed under the prompt's name.
    let o = run(echo(&home).args([
        "prompt",
        &echo_target(),
        "poster",
        "--save-dir",
        dir.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let poster = dir.join("poster-1.png");
    assert_eq!(
        o.stdout,
        format!(
            "user: Caption this.\nuser: [image saved to {}, 4 KB]\n",
            poster.display()
        )
    );
    assert_eq!(std::fs::read(&poster).unwrap(), shot_png());
}

#[test]
fn json_is_the_servers_result_untouched() {
    let home = temp_home("media-json");
    let o = run(echo(&home).args(["--json", "call", &echo_target(), "shot"]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        v,
        json!({"content": [
            {"type": "text", "text": "done"},
            {"type": "image", "data": STANDARD.encode(shot_png()), "mimeType": "image/png"},
        ]})
    );
}

#[test]
fn json_with_save_dir_carries_the_path_and_no_data() {
    let home = temp_home("media-json-save");
    let dir = home.join("shots");
    let o = run(echo(&home).args([
        "--json",
        "--save-dir",
        dir.to_str().unwrap(),
        "call",
        &echo_target(),
        "shot",
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(v["content"][0], json!({"type": "text", "text": "done"}));
    let path = dir.join("shot-1.png");
    assert_eq!(
        v["content"][1],
        json!({"type": "image", "path": path.to_str().unwrap(), "bytes": 4096, "mimeType": "image/png"})
    );
    assert!(!o.stdout.contains("iVBOR"), "{}", o.stdout);
    assert_eq!(std::fs::read(&path).unwrap(), shot_png());
}

#[test]
fn shell_honours_the_global_flag() {
    let home = temp_home("media-shell");
    let dir = home.join("shots");
    let target = echo_target();
    let mut child = echo(&home)
        .args(["--save-dir", dir.to_str().unwrap(), "shell", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"call shot\ncall shot\nprompt poster\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout,
        format!(
            "done\n[image saved to {}, 4 KB]\ndone\n[image saved to {}, 4 KB]\n\
             user: Caption this.\nuser: [image saved to {}, 4 KB]\n",
            dir.join("shot-1.png").display(),
            dir.join("shot-2.png").display(),
            dir.join("poster-1.png").display(),
        )
    );
    assert_eq!(std::fs::read(dir.join("shot-2.png")).unwrap(), shot_png());

    // Under --json every line is the object, with the file in place of the data.
    let mut child = echo(&home)
        .args([
            "--json",
            "--save-dir",
            dir.to_str().unwrap(),
            "shell",
            &target,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"call shot\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(
        v["content"][1]["path"],
        dir.join("shot-3.png").to_str().unwrap()
    );
    assert_eq!(v["content"][1]["bytes"], 4096);
    assert!(v["content"][1].get("data").is_none());
}

#[test]
fn read_files_a_blob_instead_of_refusing_it() {
    let s = start(Mode::Stateless);
    let home = temp_home("media-read");
    let dir = home.join("out");

    let o = run(echo(&home).args([
        "read",
        &s.url,
        "file:///logo.png",
        "--save-dir",
        dir.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let logo = dir.join("logo-1.png");
    assert_eq!(
        o.stdout,
        format!("[resource saved to {}, 8 B]\n", logo.display())
    );
    assert_eq!(std::fs::read(&logo).unwrap(), PNG_MAGIC);

    let o = run(echo(&home).args([
        "--json",
        "read",
        &s.url,
        "file:///logo.png",
        "--save-dir",
        dir.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v: Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        v["contents"][0],
        json!({"uri": "file:///logo.png", "mimeType": "image/png",
               "path": dir.join("logo-2.png").to_str().unwrap(), "bytes": 8})
    );

    // Text is still text.
    let o = run(echo(&home).args([
        "read",
        &s.url,
        "file:///readme.md",
        "--save-dir",
        dir.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.stdout, "# fake-mcp\nA readme.\n");
}

#[test]
fn save_dir_is_checked_before_anything_is_sent() {
    let home = temp_home("media-usage");

    let o = run(echo(&home).args([
        "--save-dir",
        home.to_str().unwrap(),
        "tools",
        &echo_target(),
    ]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("--save-dir applies to call, prompt, read, shell and tasks"),
        "{}",
        o.stderr
    );

    let file = home.join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let o = run(echo(&home).args([
        "--json",
        "call",
        &echo_target(),
        "shot",
        "--save-dir",
        file.to_str().unwrap(),
    ]));
    assert_eq!(o.code, 2, "{}", o.stderr);
    let e: Value = serde_json::from_str(&o.stderr).unwrap();
    assert_eq!(e["error"]["kind"], "usage");
    assert!(
        e["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--save-dir"),
        "{}",
        o.stderr
    );
}

/// What the terminal showed, with the carriage returns it adds taken back out
/// and the newline that ends the last line with them: every assertion below is
/// about what the line holds, not that it is a line.
fn on_screen(home: &Path, args: &[&str], env: &[(&str, &str)]) -> String {
    let shown = common::under_pty(home, args, env);
    String::from_utf8_lossy(&shown)
        .strip_suffix('\n')
        .unwrap_or_default()
        .to_string()
}

/// iTerm2 and WezTerm draw the PNG below its placeholder, in one OSC 1337
/// sequence; kitty and Ghostty in APC `_G` chunks; every other terminal, and
/// every pipe, gets the placeholder alone.
#[test]
fn a_png_is_drawn_inline_only_on_a_terminal_that_says_it_can() {
    if !cfg!(unix) || !cfg!(feature = "rich") {
        eprintln!("skipped: needs `script` to make a pseudo-terminal, and the rich feature");
        return;
    }
    let home = temp_home("media-inline");
    let target = echo_target();
    let call = ["call", target.as_str(), "shot"];
    let encoded = STANDARD.encode(shot_png());
    let placeholder = "done\n[image image/png, 4 KB]\n";

    for program in ["iTerm.app", "WezTerm"] {
        let shown = on_screen(&home, &call, &[("TERM_PROGRAM", program)]);
        assert!(
            shown.starts_with(&format!("{placeholder}\u{1b}]1337;File=inline=1;size=4096")),
            "{program}: {}",
            &shown[..shown.len().min(120)]
        );
        assert!(shown.ends_with(&format!(":{encoded}\u{7}")), "{program}");
    }

    for (var, value) in [
        ("TERM_PROGRAM", "ghostty"),
        ("KITTY_WINDOW_ID", "1"),
        ("TERM", "xterm-kitty"),
    ] {
        let shown = on_screen(&home, &call, &[(var, value)]);
        assert!(
            shown.starts_with(&format!("{placeholder}\u{1b}_Ga=T,f=100,m=1;")),
            "{var}={value}: {}",
            &shown[..shown.len().min(120)]
        );
        assert!(shown.ends_with("\u{1b}\\"), "{var}={value}");
        let carried: String = shown[placeholder.len()..]
            .split("\u{1b}\\")
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| chunk.split_once(';').unwrap().1)
            .collect();
        assert_eq!(carried, encoded, "{var}={value}");
    }

    // A prompt's message carries its role in front of the placeholder.
    let shown = on_screen(
        &home,
        &["prompt", target.as_str(), "poster"],
        &[("TERM_PROGRAM", "iTerm.app"), ("ECHO_SERVER_PROMPTS", "1")],
    );
    assert!(
        shown.contains(
            "user: Caption this.\nuser: [image image/png, 4 KB]\n\u{1b}]1337;File=inline=1;size=4096"
        ),
        "{}",
        &shown[..shown.len().min(160)]
    );

    for env in [
        vec![],
        vec![("TERM_PROGRAM", "Apple_Terminal")],
        // A multiplexer's passthrough is not something to guess at.
        vec![
            ("TERM_PROGRAM", "iTerm.app"),
            ("TMUX", "/tmp/tmux/default,1,0"),
        ],
        // Nothing is drawn for the reader that asked for the piped bytes.
        vec![("TERM_PROGRAM", "iTerm.app"), ("MCPDIAL_PLAIN", "1")],
    ] {
        let shown = on_screen(&home, &call, &env);
        assert_eq!(shown, placeholder.trim_end(), "{env:?}");
    }
}

/// The agent surface: a pipe is the placeholder whatever terminal spawned it.
#[test]
fn a_pipe_is_never_drawn_on_however_the_terminal_names_itself() {
    let home = temp_home("media-inline-piped");
    for (var, value) in [("TERM_PROGRAM", "iTerm.app"), ("KITTY_WINDOW_ID", "1")] {
        let o = run(echo(&home)
            .env(var, value)
            .args(["call", &echo_target(), "shot"]));
        assert_eq!(o.code, 0, "{}", o.stderr);
        assert_eq!(o.stdout, "done\n[image image/png, 4 KB]\n", "{var}={value}");
    }
}
