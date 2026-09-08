//! One session, many commands: naming results, retrying, editing, filtering.

use crate::common::{echo_command, mcpdial, run, start, temp_home, Mode, Out};
use crate::elicitation::elicits;
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

/// `exit` has always ended the shell; the help the shell prints has to say so,
/// since a reader who only ever sees `quit` cannot know the other word works.
#[test]
fn exit_ends_the_shell_and_the_help_says_both_words() {
    let home = temp_home("shell-exit");
    let target = format!("stdio:{}", echo_command());

    let (stdout, stderr, code) = shell(&home, &target, false, "call count\nexit\ncall count\n");
    assert_eq!(code, Some(0), "{stderr}");
    assert!(stdout.contains("count=1\n"), "{stdout}");
    assert!(!stdout.contains("count=2"), "exit stops reading: {stdout}");

    let (_, help, _) = shell(&home, &target, false, "help\n");
    assert!(help.contains("quit (or exit)"), "{help}");
}

#[test]
fn shell_keeps_one_session_alive() {
    let home = temp_home("shell");
    let target = format!("stdio:{}", echo_command());

    // Separate invocations are separate processes: the counter never gets past 1.
    for _ in 0..2 {
        let o = run(mcpdial(&home).args(["call", &target, "count"]));
        assert_eq!(o.stdout.trim(), "count=1");
    }

    // The results are numbered as they print, so the lines after them can name
    // one instead of running it again.
    let saved = home.join("out.txt");
    let script = format!(
        "# a comment\ncall count\ncall count {{}}\ntools\ncall nope\nraw tools/list\n\
         call count\nshow 1\nsave 2 {}\nretry echo message=again\nquit\ncall count\n",
        saved.display()
    );
    let mut child = mcpdial(&home)
        .args(["shell", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("count=1\n"), "{stdout}");
    assert!(stdout.contains("count=2\n"), "{stdout}");
    assert!(
        stdout.contains("count=3\n"),
        "same process throughout: {stdout}"
    );
    assert!(!stdout.contains("count=4"), "quit stops reading: {stdout}");
    assert!(stdout.contains("5 tool(s):"), "{stdout}");
    assert!(
        stdout.contains("\"name\": \"count\""),
        "raw output: {stdout}"
    );
    assert!(
        stderr.contains("MCP error -32602"),
        "errors go to stderr and do not end the session: {stderr}"
    );
    assert_eq!(
        stdout.matches("count=1\n").count(),
        2,
        "`show 1` prints the first result again: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&saved).unwrap(),
        "count=2",
        "`save 2` writes the second result"
    );
    assert!(
        stdout.contains("Echo: again"),
        "`retry echo message=again` sends the pairs it was given: {stdout}"
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "a failed command in a script is reported in the exit code"
    );

    let mut child = mcpdial(&home)
        .args(["--json", "shell", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"call count\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["content"][0]["text"], "count=1");
}

/// One `shell` session fed a script, with a wall clock around it: a shell left
/// waiting for input nobody is going to type fails the test instead of stalling
/// the suite.
fn shell_script(cmd: &mut Command, script: &str) -> Out {
    use std::time::{Duration, Instant};
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                child.kill().ok();
                child.wait().ok();
                panic!("the shell was still running after 60s");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let o = child.wait_with_output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

/// The three ways of naming a result, what `save` does with each kind of one,
/// and what it says when it will not write.
#[test]
fn shell_names_results_and_saves_them() {
    let home = temp_home("shell-history");
    let target = format!("stdio:{}", echo_command());
    let text = home.join("text.txt");
    let shot = home.join("shot.png");
    let script = format!(
        "call echo {{\"message\":\"one\"}}\ncall count\ncall shot\n\
         show 1\nshow $2\nshow _\nsave 2 {text}\nsave 3 {shot}\n\
         show 9\nshow nope\nsave 1 {text}\nsave 1 {dir}\nquit\n",
        text = text.display(),
        shot = shot.display(),
        dir = home.display(),
    );
    let o = shell_script(mcpdial(&home).args(["shell", &target]), &script);

    assert_eq!(
        o.stdout.matches("Echo: one").count(),
        2,
        "`show 1` prints the first result again: {}",
        o.stdout
    );
    assert_eq!(
        o.stdout.matches("count=1").count(),
        2,
        "`show $2` names the second: {}",
        o.stdout
    );
    assert_eq!(
        std::fs::read_to_string(&text).unwrap(),
        "count=1",
        "text is saved as text"
    );
    let bytes = std::fs::read(&shot).unwrap();
    assert_eq!(
        &bytes[..4],
        b"\x89PNG",
        "a binary block is saved as its bytes"
    );
    assert_eq!(bytes.len(), 4096, "all of them, not the placeholder line");

    assert!(
        o.stderr.contains("there is no result 9"),
        "a number nobody printed: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("not a result number"),
        "a word that is no number: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("already exists"),
        "a file already there is not written over: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("is a directory"),
        "and neither is a directory: {}",
        o.stderr
    );
    assert_eq!(o.code, 1, "the refusals are counted: {}", o.stderr);

    // With no file named, one is named after the tool and the media type. It
    // lands in the working directory, so give the session one of its own.
    let named = temp_home("shell-save-named");
    let o = shell_script(
        mcpdial(&home).args(["shell", &target]).current_dir(&named),
        "call shot\nsave _\nquit\n",
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("wrote 4,096 bytes to shot.png"),
        "{}",
        o.stdout
    );
    assert_eq!(std::fs::read(named.join("shot.png")).unwrap().len(), 4096);
}

/// `retry` sends the last call again with one argument different, and takes the
/// types from the tool's schema exactly as a typed `call` does.
#[test]
fn shell_retries_the_last_call_with_one_argument_changed() {
    let home = temp_home("shell-retry");
    let target = format!("stdio:{}", echo_command());
    let session = || {
        let mut c = mcpdial(&home);
        c.env("ECHO_SERVER_TYPES", "1").args(["shell", &target]);
        c
    };
    let o = shell_script(
        &mut session(),
        "call typed text=a count=1\nretry count=2\nretry typed flag=true\n\
         retry echo message=hi\nretry\nquit\n",
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains(r#"{"count":1,"text":"a"}"#),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains(r#"{"count":2,"text":"a"}"#),
        "the arguments nobody changed are kept, and 2 is still a number: {}",
        o.stdout
    );
    assert!(
        o.stdout.contains(r#"{"count":2,"flag":true,"text":"a"}"#),
        "a named tool retries its own last call: {}",
        o.stdout
    );
    assert_eq!(
        o.stdout.matches("Echo: hi").count(),
        2,
        "a bare `retry` sends the last call over: {}",
        o.stdout
    );
    assert!(
        o.stderr.contains(r#"call typed {"count":2,"text":"a"}"#),
        "what it re-ran is said the way it would be typed: {}",
        o.stderr
    );

    // Nothing to retry yet says so rather than sending anything.
    let o = shell_script(&mut session(), "retry\nedit\nquit\n");
    assert_eq!(o.code, 1);
    assert_eq!(
        o.stderr.matches("no call in this session yet").count(),
        2,
        "{}",
        o.stderr
    );
}

/// `edit` opens a call's arguments in `$EDITOR` and sends what it left; an editor
/// that fails, or leaves nothing behind, sends nothing.
#[cfg(unix)]
#[test]
fn shell_edit_sends_what_the_editor_left() {
    let home = temp_home("shell-edit");
    let target = format!("stdio:{}", echo_command());
    let write = |name: &str, body: &str| {
        let path = home.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        path
    };
    let good = write(
        "good.sh",
        "#!/bin/sh\nprintf '{\"message\": \"edited\"}' > \"$1\"\n",
    );
    let refuses = write("bad.sh", "#!/bin/sh\nexit 3\n");
    let empties = write("empty.sh", "#!/bin/sh\n: > \"$1\"\n");

    let script = "call echo {\"message\":\"first\"}\nedit 1\nquit\n";
    let o = shell_script(
        mcpdial(&home).args(["shell", &target]).env("EDITOR", &good),
        script,
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("Echo: edited"), "{}", o.stdout);
    assert!(
        o.stderr.contains(r#"call echo {"message":"edited"}"#),
        "{}",
        o.stderr
    );

    for (editor, said) in [
        (&refuses, "exited without saving"),
        (&empties, "left empty"),
    ] {
        let o = shell_script(
            mcpdial(&home)
                .args(["shell", &target])
                .env("EDITOR", editor),
            script,
        );
        assert_eq!(o.code, 1, "{}", o.stderr);
        assert!(o.stderr.contains(said), "{}", o.stderr);
        assert_eq!(
            o.stdout.matches("Echo:").count(),
            1,
            "nothing was sent a second time: {}",
            o.stdout
        );
    }

    // Nothing to open the arguments with is said before anything is written.
    let o = shell_script(
        mcpdial(&home)
            .args(["shell", &target])
            .env_remove("EDITOR")
            .env_remove("VISUAL"),
        script,
    );
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("set $EDITOR"), "{}", o.stderr);
}

/// A `| ...` at the end of a shell line prints one part of the result instead of
/// all of it, after a fresh command and after the number of one already printed.
#[test]
fn shell_filters_a_result_with_a_path_expression() {
    let home = temp_home("shell-filter");
    let target = format!("stdio:{}", echo_command());
    let o = shell_script(
        mcpdial(&home).args(["shell", &target]),
        concat!(
            "raw tools/list | .tools[].name\n",
            "call echo {\"message\":\"hi\"} | .content[0].text\n",
            // A bar inside a JSON string is part of the string, not a filter.
            "call echo {\"message\":\"a|b\"} | .content[0].text\n",
            "_ | .content[0].type\n",
            "show 2 | .content[].text\n",
            "$2 | .\n",
            // A retry carries the filter on the end of the line it hands back.
            "retry message=again | .content[0].text\n",
            "call echo {\"message\":\"z\"} | .nope.deep\n",
            "quit\n",
        ),
    );
    assert_eq!(
        o.code, 0,
        "a path naming nothing is not a failure: {}",
        o.stderr
    );
    assert_eq!(
        o.stdout,
        concat!(
            "echo\nfail\ncount\nstrict\nshot\n",
            "Echo: hi\n",
            "Echo: a|b\n",
            "text\n",
            "Echo: hi\n",
            "{\"content\":[{\"text\":\"Echo: hi\",\"type\":\"text\"}]}\n",
            "Echo: again\n",
        ),
        "stderr was: {}",
        o.stderr
    );
    assert!(
        o.stderr
            .contains(r#"call echo {"message":"again"} | .content[0].text"#),
        "the line a retry hands back carries the filter, ready to paste: {}",
        o.stderr
    );
    assert!(
        o.stderr
            .contains(".nope.deep matched nothing in this result"),
        "a path that matches nothing prints nothing and says so: {}",
        o.stderr
    );
}

/// What the small grammar will not read goes to `jq`, and what neither of them
/// can read is refused before anything is sent.
#[test]
fn shell_hands_the_rest_to_jq_and_refuses_what_neither_can_read() {
    let home = temp_home("shell-filter-jq");
    let target = format!("stdio:{}", echo_command());

    // `count` answers with how many calls it has had, so the number it comes
    // back with is the proof that an unreadable filter sent nothing.
    let o = shell_script(
        mcpdial(&home).args(["shell", &target]),
        "call count | .content[\ntools | .name\n| .name\ncall count\nquit\n",
    );
    assert_eq!(o.code, 1, "{}", o.stderr);
    assert!(o.stderr.contains("is not a path"), "{}", o.stderr);
    assert!(
        o.stderr.contains("tools prints no result to filter"),
        "a filter needs a result to filter: {}",
        o.stderr
    );
    assert!(
        o.stderr.contains("a filter needs a command in front of it"),
        "and a command in front of it: {}",
        o.stderr
    );
    assert_eq!(
        o.stdout, "count=1\n",
        "the filter nobody could read sent nothing: {}",
        o.stdout
    );

    let script = concat!(
        "call echo {\"message\":\"hi\"} | jq -r '.content[0].text'\n",
        "call echo {\"message\":\"hi\"} | jq '.content['\n",
        "quit\n",
    );
    let o = shell_script(mcpdial(&home).args(["shell", &target]), script);
    if std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join("jq").is_file() || dir.join("jq.exe").is_file())
    {
        assert_eq!(o.stdout, "Echo: hi\n", "{}", o.stderr);
        assert!(
            o.stderr.contains("jq failed"),
            "a filter jq itself refuses is reported as jq's refusal: {}",
            o.stderr
        );
    } else {
        assert_eq!(o.stdout, "", "{}", o.stdout);
        assert_eq!(
            o.stderr.matches("jq is not installed").count(),
            2,
            "{}",
            o.stderr
        );
    }
    assert_eq!(o.code, 1, "{}", o.stderr);
}

/// Every way a line can be wrong should answer with the shape that was wanted.
#[test]
fn shell_explains_the_shape_it_expected() {
    let home = temp_home("shell-hints");
    let target = format!("stdio:{}", echo_command());
    let script = concat!(
        "echo\n",                            // a tool name typed as if it were a command
        "call echo\n",                       // a required argument left out
        "call echo [www.x.com](http://x)\n", // arguments that are not JSON at all
        "call echo {message:x}\n",           // an object missing its quotes
        "call ech {\"message\":\"x\"}\n",    // a tool name with a typo
        "tolls\n",                           // a command with a typo
        "schema echo\n",
        "help echo\n",
        "quit\n",
    );
    let (stdout, stderr, code) = shell(&home, &target, false, script);

    // A bare tool name is the commonest mistake, and it names the fix.
    assert!(stderr.contains("echo is a tool, not a command"), "{stderr}");
    // Every failed call answers with a line that would have worked.
    assert_eq!(
        stderr
            .matches(r#"usage: call echo {"message": "<string>"}"#)
            .count(),
        5,
        "bare name, missing argument, two unparseable arguments and `help echo`: {stderr}"
    );
    assert!(
        stderr.contains("message: string (required)"),
        "and the parameter list: {stderr}"
    );
    // Unparseable arguments quote what actually arrived.
    assert!(
        stderr.contains(r#""[www.x.com](http://x)" is not JSON"#),
        "{stderr}"
    );
    // An object that only lacks its quotes gets them back.
    assert!(
        stderr.contains(r#"did you mean {"message": "x"}?"#),
        "{stderr}"
    );
    // Near misses are named, for tools and for commands.
    assert!(stderr.contains("did you mean echo?"), "{stderr}");
    assert!(stderr.contains("did you mean tools?"), "{stderr}");
    // schema prints the tool's own schema; help prints the readable form.
    assert!(
        stdout.contains(r#""required": ["#) && stdout.contains(r#""message""#),
        "{stdout}"
    );
    assert_eq!(code, Some(1), "a script with failures still exits 1");

    // A schema complaint that arrives as a failed *result* rather than a JSON-RPC
    // error is the same mistake, and gets the same answer.
    let (stdout, stderr, _) = shell(&home, &target, false, "call strict {}\n");
    assert!(stdout.contains("Required at pageId"), "{stdout}");
    assert!(
        stderr.contains(r#"usage: call strict {"pageId": <number>}"#),
        "a failed result still explains itself: {stderr}"
    );
    // A tool that just failed does not get a schema dumped under it.
    let (_, stderr, _) = shell(&home, &target, false, "call fail {}\n");
    assert!(!stderr.contains("usage:"), "{stderr}");

    // In --json the hint rides along on the error object, one line per command.
    let (stdout, _, _) = shell(&home, &target, true, "call echo\ncall nope {}\n");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert!(
        lines[0]["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with(r#"usage: call echo {"message": "<string>"}"#),
        "{stdout}"
    );
    assert!(
        lines[1]["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("lists all 5"),
        "{stdout}"
    );
}

/// Pipe `script` into `mcpdial shell` and collect everything it said.
pub(crate) fn shell(
    home: &std::path::Path,
    target: &str,
    json: bool,
    script: &str,
) -> (String, String, Option<i32>) {
    let mut cmd = mcpdial(home);
    if json {
        cmd.arg("--json");
    }
    let mut child = cmd
        .args(["shell", target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

#[test]
fn the_shell_reaches_resources_and_prompts() {
    let s = start(Mode::Stateful);
    let home = temp_home("shell-resources");
    // A read and a prompt are numbered results too, so they can be shown again
    // and saved: text as text, a blob as the bytes it arrived as.
    let notes = home.join("summary.txt");
    let logo = home.join("logo.png");
    let script = format!(
        "resources\nread file:///readme.md\nprompts\n\
         prompt summarize {{\"text\":\"a memo\"}}\nshow 1\nsave 2 {notes}\n\
         read file:///logo.png\nsave 3 {logo}\nread\nquit\n",
        notes = notes.display(),
        logo = logo.display(),
    );
    let (stdout, stderr, code) = shell(&home, &s.url, false, &script);
    assert!(
        stdout.contains("2 resource(s):") && stdout.contains("file:///logo.png"),
        "{stdout}"
    );
    assert!(
        stdout.contains("1 template(s):") && stdout.contains("file:///notes/{name}.md"),
        "{stdout}"
    );
    assert!(stdout.contains("# fake-mcp"), "{stdout}");
    assert!(stdout.contains("2 prompt(s):"), "{stdout}");
    assert!(stdout.contains("user: Summarize this: a memo"), "{stdout}");
    assert!(stderr.contains("read needs a resource URI"), "{stderr}");
    assert_eq!(code, Some(1), "the read with no URI failed");
    assert_eq!(
        stdout.matches("# fake-mcp").count(),
        2,
        "`show 1` prints the resource again: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&notes).unwrap(),
        "user: Summarize this: a memo\nassistant: Sure.",
        "a prompt is saved as the messages it printed"
    );
    assert_eq!(
        &std::fs::read(&logo).unwrap()[..4],
        b"\x89PNG",
        "a blob is saved as the bytes it arrived as"
    );

    let (stdout, _, _) = shell(&home, &s.url, true, "resources\nprompt greet\nquit\n");
    let lines: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert_eq!(lines[0]["resources"].as_array().unwrap().len(), 2);
    assert_eq!(lines[0]["resourceTemplates"][0]["name"], "note");
    assert_eq!(lines[1]["messages"][0]["content"]["text"], "Hello.");
}

#[test]
fn the_shell_elicit_command_answers_every_call_after_it() {
    let home = temp_home("elicit-shell");
    let target = format!("stdio:{}", echo_command());

    let script = "call echo {\"message\":\"one\"}\n\
                  elicit {\"confirm\":true,\"region\":\"eu\"}\n\
                  call echo {\"message\":\"two\"}\n\
                  elicit\n\
                  quit\n";
    let mut child = elicits(&home, "form")
        .args(["-v", "shell", &target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Before any answers were given the elicitation is declined; after, the
    // same call is accepted with them, for the rest of the session.
    assert!(
        stdout.contains(r#"elicited {"action":"decline"}"#),
        "{stdout}"
    );
    assert!(
        stdout.contains(r#"elicited {"action":"accept","content":{"confirm":true,"region":"eu"}}"#),
        "{stdout}"
    );
    assert!(
        stdout.contains("2 answer(s) ready to elicit with"),
        "{stdout}"
    );
    assert!(stderr.contains("elicit needs a JSON object"), "{stderr}");
    // A shell can be handed answers at any point, so it offers to fill in a
    // form from the start, before any have been typed.
    assert!(
        stderr.contains(r#""capabilities":{"elicitation":{"form":{},"url":{}}}"#),
        "{stderr}"
    );
}
