//! What `help` prints, and the usage lines a mistyped command is answered with.

pub(crate) const SHELL_COMMANDS: &[&str] = &[
    "tools",
    "schema",
    "call",
    "resources",
    "read",
    "prompts",
    "prompt",
    "raw",
    "elicit",
    "show",
    "save",
    "retry",
    "edit",
    "subscribe",
    "unsubscribe",
    "subscriptions",
    "listen",
    "info",
    "help",
    "quit",
    "exit",
];

pub(crate) const SHELL_SUMMARY: &str =
    "commands: tools, schema TOOL, call TOOL {\"arg\": \"value\"}, \
     resources, read URI, prompts, prompt NAME, raw METHOD, elicit {\"x\": 1}, show N, \
     save N, retry, edit, subscribe URI, unsubscribe URI, subscriptions, listen, info, \
     help, quit (or exit)";

pub(crate) const SHELL_HELP: &str = r#"commands (one per line; # starts a comment):
  tools [--long]             every tool this server offers
  schema TOOL                one tool's full JSON input schema
  help [TOOL]                this list, or one tool's parameters
  call TOOL {"arg": "value"} call a tool; arguments are one JSON object, default {}
  call TOOL arg=value        the same, as pairs the tool's schema types
  resources [--long]         every resource, then every URI template
  read URI                   one resource's contents
  prompts [--long]           every prompt this server offers
  prompt NAME {"arg": "..."} expand a prompt into its messages
  raw METHOD {"json": ...}   send any JSON-RPC method; allow and deny lists do not apply
  elicit {"arg": "value"}    answers for whatever the server elicits from here on
  show N                     print result N again; _ is the last, $3 the third
  save N [FILE]              write result N to a file, named after the tool by default
  retry [TOOL] [key=value]   send the last call again, with those arguments changed
  edit [N]                   open a call's arguments in $EDITOR and send them again
  subscribe URI [FILE]       follow a resource, into a file or onto the screen
  unsubscribe URI            stop following one
  subscriptions              what this session follows
  listen [SECONDS]           hold a stream open for updates, where the revision needs one
  info                       the initialize result
  quit (or exit)             close the session

A command that prints a result can end with | PATH (.key, .key.sub, .[0], .[]) or
with | jq ARGS. _ and $3 name a result on their own, which a filter can follow.

At a terminal: Up and Down walk the history and ^C abandons the line being typed.
Tab completes a command, the tool, prompt or resource URI it takes, and the keys
and values of a call's or a prompt's arguments."#;

/// What a `save` line calls itself in the errors it makes, where `--output`
/// names the flag.
pub(crate) const SAVE: &str = "save";

pub(crate) const SHOW_USAGE: &str = "usage: show N   (_ is the last result, $3 and 3 the third)";

pub(crate) const SAVE_USAGE: &str =
    "usage: save N [FILE]   (without a file, one named after the tool and its media type)";

pub(crate) const RETRY_USAGE: &str =
    "usage: retry [TOOL] [key=value ...]   (the last call, with those arguments changed)";

pub(crate) const EDIT_USAGE: &str =
    "usage: edit [N]   (opens a call's arguments in $EDITOR and runs it again on save)";

/// The commands a `| ...` can follow, for the answer to one that prints no
/// result to filter.
pub(crate) const FILTER_APPLIES_TO: &str =
    "a filter follows a command that prints a result: call, read, prompt, raw, retry, edit, \
     show N, _ and $N";
