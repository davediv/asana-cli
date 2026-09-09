//! The prose pieces of the help screens that clap cannot express: the root
//! description and examples, usage lines, command groupings, the ARGUMENTS
//! sections, the help topics (`asana help exit-codes`, `asana help
//! formatting`), and the LEARN MORE footer. `cli.rs` renders them; headings are painted with the shared
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
        "Learn about output formatting using `asana help formatting`".to_string(),
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

/// A help topic: `asana help <name>` prints its body, then `EXAMPLES` when
/// it has any; the root screen lists `name: short` under `HELP TOPICS`,
/// like gh.
pub struct Topic {
    pub name: &'static str,
    pub short: &'static str,
    pub body: Body,
    /// Example invocations (`$ ...` lines and `# comments`), printed under a
    /// plain `EXAMPLES` heading after the body as `gh help formatting` does;
    /// empty for none.
    pub example: &'static str,
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
        body: Body::Text("`ASANA_TOKEN`, `ASANA_ACCESS_TOKEN` (in order of precedence): a personal access token used to\n\
               authenticate with Asana. Setting either overrides `token` in the config file. Create one at\n\
               <https://app.asana.com/0/my-apps>.\n\
               \n\
               `ASANA_WORKSPACE`: the gid or name of the workspace commands operate in. Overrides `workspace`\n\
               in the config file; the `--workspace` flag overrides both. When nothing is set and the token\n\
               belongs to exactly one workspace, that workspace is used.\n\
               \n\
               `XDG_CONFIG_HOME`: the directory the config file is read from, as `asana/config.toml` under it.\n\
               If not set, `$HOME/.config/asana/config.toml` is used. The file may hold `token = \"...\"` and\n\
               `workspace = \"...\"`.\n\
               \n\
               `ASANA_BASE_URL`: the API base URL to use instead of `https://app.asana.com/api/1.0`, for\n\
               testing against a mock server.\n\
               \n\
               `NO_COLOR`: set to any value to avoid printing ANSI escape sequences for color output.\n\
               \n\
               `TERM`: set to `dumb` to disable color output.\n\
               \n\
               `COLUMNS`: the width to lay tables out for when the terminal size cannot be detected\n\
               (default 80).\n"),
        example: "",
    },
    Topic {
        name: "exit-codes",
        short: "Exit codes used by asana",
        body: Body::Text("asana follows normal conventions regarding exit codes.\n\
               \n\
               - If a command completes successfully, the exit code will be 0\n\
               \n\
               - If a command fails for any reason, the exit code will be 1\n\
               \n\
               - If a command is running but gets cancelled, the exit code will be 2\n\
               \n\
               - If a command requires authentication, the exit code will be 4\n\
               \n\
               NOTE: Usage errors, such as an unknown flag or a missing argument, exit with 1 like any other\n\
               failure. Completing a task that is already complete counts as success and exits with 0.\n"),
        example: "",
    },
    Topic {
        name: "formatting",
        short: "Formatting options for JSON and piped output from asana",
        body: Body::Text("By default, the result of `asana` commands is output in line-based plain text format: lists as\n\
               tables and views as a labelled summary, laid out for the width of the terminal and colored\n\
               when stdout is a terminal.\n\
               \n\
               When stdout is not a terminal (a pipe, a file, a `$(...)` substitution), the same commands\n\
               print text meant for other programs instead. Lists are tab-separated with no header, one item\n\
               per line, with ISO-8601 timestamps; views are `key:<TAB>value` lines, then a line holding\n\
               `--`, then the body as written. Nothing is colored or truncated, so `cut`, `awk` and `grep`\n\
               work on the output as is. Pass `--no-color` or set `NO_COLOR` to drop color on a terminal\n\
               as well.\n\
               \n\
               Commands that read from Asana (`list` and `view`) also support the `--json` flag, which prints\n\
               the objects the API returned instead: an array for lists, one object for views, pretty-printed\n\
               and never colored. The flag takes no field list; every field the command fetched is included,\n\
               so pipe the output into `jq` to select or reshape it. To learn about jq query syntax, see:\n\
               <https://jqlang.github.io/jq/manual/>\n\
               \n\
               Commands that create something (`task create`, `task comment`) print its permalink URL alone\n\
               on stdout, and every command's progress and status lines (`\u{2713} Completed task ...`) go to\n\
               stderr, so `$(asana task create ...)` captures the URL and nothing else.\n"),
        example: "# Default output format\n\
                  $ asana task list\n\
                  \n\
                  Showing 3 of 3 open tasks assigned to you in Acme\n\
                  \n\
                  ID                TITLE                    PROJECT  DUE       UPDATED\n\
                  1211234567890123  Fix the login page       Roadmap  today     about 1 day ago\n\
                  1211234567890124  Write the release notes  Roadmap  Sep 12    about 2 days ago\n\
                  1211234567890125  Renew the certificate             tomorrow  about 2 days ago\n\
                  \n\
                  \n\
                  # Piped: one task per line, tab-separated, no header\n\
                  $ asana task list | cut -f1,3\n\
                  1211234567890123\tFix the login page\n\
                  1211234567890124\tWrite the release notes\n\
                  1211234567890125\tRenew the certificate\n\
                  \n\
                  \n\
                  # Adding the --json flag\n\
                  $ asana task list --json\n\
                  [\n\
                  \x20 {\n\
                  \x20   \"assignee\": {\n\
                  \x20     \"gid\": \"1200000000000001\",\n\
                  \x20     \"name\": \"Jane Doe\"\n\
                  \x20   },\n\
                  \x20   \"completed\": false,\n\
                  \x20   \"due_on\": \"2026-09-09\",\n\
                  \x20   \"gid\": \"1211234567890123\",\n\
                  \x20   \"name\": \"Fix the login page\",\n\
                  \x20   ...\n\
                  \x20 },\n\
                  \x20 ...\n\
                  ]\n\
                  \n\
                  \n\
                  # Selecting fields from the JSON with jq\n\
                  $ asana task list --json | jq -r '.[].name'\n\
                  Fix the login page\n\
                  Write the release notes\n\
                  Renew the certificate\n\
                  \n\
                  \n\
                  # A created task's URL is all that reaches stdout\n\
                  $ url=$(asana task create --title \"Fix the login page\" --project Roadmap)\n\
                  \n\
                  Creating task in Roadmap\n\
                  \n\
                  $ echo \"$url\"\n\
                  https://app.asana.com/0/1200000000000000/1211234567890123\n",
    },
    Topic {
        name: "reference",
        short: "A comprehensive reference of all asana commands",
        body: Body::Reference,
        example: "",
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
            for line in long.lines().chain(t.example.lines()) {
                assert!(line.len() <= 110, "{}: line too long: {line}", t.name);
                assert!(
                    line.is_empty() || !line.ends_with(' '),
                    "{}: trailing space: {line:?}",
                    t.name
                );
            }
            if !t.example.is_empty() {
                assert!(
                    t.example.starts_with("# "),
                    "{} example opens with a comment",
                    t.name
                );
                assert!(
                    t.example.ends_with('\n'),
                    "{} example must end with a newline",
                    t.name
                );
            }
        }
        assert!(topic("formatting").is_some_and(|t| !t.example.is_empty()));
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
        assert_eq!(lines.len(), 5);
        for t in TOPICS.iter().filter(|t| matches!(t.body, Body::Text(_))) {
            assert!(lines
                .iter()
                .any(|l| l.contains(&format!("asana help {}", t.name))));
        }
    }
}
