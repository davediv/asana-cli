//! The prose pieces of the help screens that clap cannot express: the root
//! description and examples, usage lines, command groupings, the ARGUMENTS
//! sections, and the LEARN MORE footer. `cli.rs` renders them.

pub const ROOT_ABOUT: &str = "Work with an Asana workspace from the command line.";

pub const ROOT_EXAMPLES: &str = "$ asana task list\n$ asana task create --title \"Fix the login page\"\n$ asana task view 1211234567890123 --web";

pub const MANUAL_URL: &str = "https://github.com/davediv/asana-cli#readme";

pub const PAT_URL: &str = "https://app.asana.com/0/my-apps";

/// The `LEARN MORE` footer printed on every help screen.
pub fn learn_more() -> Vec<String> {
    vec![
        "Use `asana <command> <subcommand> --help` for more information about a command."
            .to_string(),
        format!("Read the manual at {MANUAL_URL}"),
        format!("Create a personal access token at {PAT_URL} and export it as ASANA_TOKEN"),
    ]
}

/// The usage line for a command path (without the leading `asana`), when it
/// differs from the default `asana <path> [flags]`.
pub fn usage(path: &[&str]) -> Option<&'static str> {
    Some(match path {
        [] => "asana <command> <subcommand> [flags]",
        ["auth"] | ["workspace"] | ["project"] | ["task"] => return None,
        ["project", "view"] => "asana project view {<gid> | <url> | <name>} [flags]",
        ["task", "view"] => "asana task view {<gid> | <url>} [flags]",
        ["task", "complete"] => "asana task complete {<gids> | <urls>} [flags]",
        ["task", "reopen"] => "asana task reopen {<gids> | <urls>} [flags]",
        ["task", "assign"] => "asana task assign {<gid> | <url>} <assignee> [flags]",
        ["task", "comment"] => "asana task comment {<gid> | <url>} [flags]",
        ["task", "delete"] => "asana task delete {<gid> | <url>} [flags]",
        _ => return None,
    })
}

/// How the subcommands of a command are grouped on its help screen. Commands
/// not named here fall into `ADDITIONAL COMMANDS`.
pub fn groups(path: &[&str]) -> Vec<(&'static str, &'static [&'static str])> {
    match path {
        [] => vec![("CORE COMMANDS", &["auth", "project", "task", "workspace"])],
        ["task"] => vec![
            ("GENERAL COMMANDS", &["create", "list"]),
            (
                "TARGETED COMMANDS",
                &["assign", "comment", "complete", "delete", "reopen", "view"],
            ),
        ],
        ["project"] => vec![
            ("GENERAL COMMANDS", &["list"]),
            ("TARGETED COMMANDS", &["view"]),
        ],
        _ => Vec::new(),
    }
}

/// The `ARGUMENTS` section for commands that take identifiers.
pub fn arguments(path: &[&str]) -> Option<&'static str> {
    match path {
        ["task"] => Some(
            "A task can be supplied as argument in any of the following formats:\n\
             - by gid, e.g. \"1211234567890123\"; or\n\
             - by URL, e.g. \"https://app.asana.com/0/1200000000000000/1211234567890123\".",
        ),
        ["project"] => Some(
            "A project can be supplied as argument in any of the following formats:\n\
             - by gid, e.g. \"1200000000000000\";\n\
             - by URL, e.g. \"https://app.asana.com/0/1200000000000000/list\"; or\n\
             - by name, e.g. \"Roadmap\" (case-insensitive; a unique prefix is enough).",
        ),
        _ => None,
    }
}
