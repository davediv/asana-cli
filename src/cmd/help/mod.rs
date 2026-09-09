//! The prose pieces of the help screens that clap cannot express: the root
//! description and examples, usage lines, command groupings, the ARGUMENTS
//! sections, the help topics (`asana help exit-codes`), and the LEARN MORE
//! footer. `cli.rs` renders them; headings are painted with the shared
//! `Palette::bold`, which emits gh's `\e[0;1;39m`.

pub const ROOT_ABOUT: &str = "Work with an Asana workspace from the command line.";

pub const ROOT_EXAMPLES: &str = "$ asana task list\n$ asana task create --title \"Fix the login page\"\n$ asana task view 1211234567890123 --web";

pub const MANUAL_URL: &str = "https://github.com/davediv/asana-cli#readme";

pub const RELEASES_URL: &str = "https://github.com/davediv/asana-cli/releases/tag";

/// The `LEARN MORE` footer printed on every help screen.
pub fn learn_more() -> Vec<String> {
    vec![
        "Use `asana <command> <subcommand> --help` for more information about a command."
            .to_string(),
        format!("Read the manual at {MANUAL_URL}"),
        "Learn about exit codes using `asana help exit-codes`".to_string(),
        "Learn about environment variables using `asana help environment`".to_string(),
    ]
}

/// The usage line for a command path, when it differs from the default
/// `asana <path> [<command>] [flags]`.
pub fn usage(path: &[&str]) -> Option<&'static str> {
    Some(match path {
        [] => "asana <command> <subcommand> [flags]",
        ["auth"] | ["workspace"] | ["project"] | ["task"] => return None,
        ["project", "view"] => "asana project view {<gid> | <url> | <name>} [flags]",
        ["task", "view"] => "asana task view {<gid> | <url>} [flags]",
        ["task", "complete"] => "asana task complete {<gids> | <urls>} [flags]",
        ["task", "reopen"] => "asana task reopen {<gids> | <urls>} [flags]",
        ["task", "assign"] => "asana task assign {<gid> | <url>} [<assignee>] [flags]",
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

/// A help topic: `asana help <name>` prints its body; the root screen lists
/// `name: short` under `HELP TOPICS`, like gh.
pub struct Topic {
    pub name: &'static str,
    pub short: &'static str,
    pub body: Body,
}

/// What a topic prints: fixed prose, or the command reference gh generates
/// from its command tree (`gh help reference`).
pub enum Body {
    Text(&'static str),
    Reference,
}

pub const TOPICS: &[Topic] = &[
    Topic {
        name: "environment",
        short: "Environment variables that can be used with asana",
        body: Body::Text("ASANA_TOKEN, ASANA_ACCESS_TOKEN (in order of precedence): a personal access token used to authenticate\n\
               with Asana. Setting either overrides `token` in the config file. Create one at\n\
               https://app.asana.com/0/my-apps.\n\
               \n\
               ASANA_WORKSPACE: the gid or name of the workspace commands operate in. Overrides `workspace` in the\n\
               config file; the `--workspace` flag overrides both. When nothing is set and the token belongs to\n\
               exactly one workspace, that workspace is used.\n\
               \n\
               XDG_CONFIG_HOME: the directory the config file is read from, as \"asana/config.toml\" under it.\n\
               If not set, \"$HOME/.config/asana/config.toml\" is used. The file may hold `token = \"...\"` and\n\
               `workspace = \"...\"`.\n\
               \n\
               ASANA_BASE_URL: the API base URL to use instead of \"https://app.asana.com/api/1.0\", for testing\n\
               against a mock server.\n\
               \n\
               NO_COLOR: set to any value to avoid printing ANSI escape sequences for color output.\n\
               \n\
               TERM: set to \"dumb\" to disable color output.\n\
               \n\
               COLUMNS: the width to lay tables out for when the terminal size cannot be detected (default 80).\n"),
    },
    Topic {
        name: "exit-codes",
        short: "Exit codes used by asana",
        body: Body::Text("asana follows normal conventions regarding exit codes.\n\
               \n\
               - If a command completes successfully, the exit code will be 0\n\
               - If a command fails for any reason, the exit code will be 1\n\
               - If a command is running but gets cancelled, the exit code will be 2\n\
               - If a command encounters an authentication issue, the exit code will be 4\n\
               \n\
               NOTE: Usage errors, such as an unknown flag or a missing argument, exit with 1 like any other\n\
               failure. Completing a task that is already complete counts as success and exits with 0.\n"),
    },
    Topic {
        name: "reference",
        short: "A comprehensive reference of all asana commands",
        body: Body::Reference,
    },
];

pub fn topic(name: &str) -> Option<&'static Topic> {
    TOPICS.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_well_formed() {
        assert!(TOPICS.windows(2).all(|w| w[0].name < w[1].name), "sorted");
        for t in TOPICS {
            assert!(
                !t.short.ends_with('.'),
                "{} short ends with a period",
                t.name
            );
            let Body::Text(long) = t.body else {
                continue;
            };
            assert!(long.ends_with('\n'), "{} must end with a newline", t.name);
            assert!(!long.ends_with("\n\n"), "{} ends with a blank line", t.name);
            for line in long.lines() {
                assert!(line.len() <= 110, "{}: line too long: {line}", t.name);
            }
        }
        assert!(topic("exit-codes").is_some());
        assert!(matches!(
            topic("reference"),
            Some(Topic {
                body: Body::Reference,
                ..
            })
        ));
        assert!(topic("nope").is_none());
    }

    #[test]
    fn learn_more_points_at_the_topics() {
        let lines = learn_more();
        assert_eq!(lines.len(), 4);
        for t in TOPICS.iter().filter(|t| matches!(t.body, Body::Text(_))) {
            assert!(lines
                .iter()
                .any(|l| l.contains(&format!("asana help {}", t.name))));
        }
    }
}
