//! The prose pieces of the help screens that clap cannot express: the root
//! description and examples, the `auth` group's description (where a new
//! user learns how to authenticate), usage lines, command groupings, the
//! ARGUMENTS sections, the help topics (`asana help exit-codes`, `asana help
//! formatting`), and the LEARN MORE footer. `cli.rs` renders them; headings
//! are painted with the shared `Palette::bold`, which emits gh's
//! `\e[0;1;39m`.
//!
//! Every line of prose here is hand-wrapped short of [`PROSE_WIDTH`] columns
//! (counting the two-space indent of a titled section), the way gh's heredoc
//! descriptions are, so nothing spills a word onto the next row of an
//! 80-column terminal. URLs are bare, never `<...>`-bracketed. The renderer
//! in `cli.rs` still breaks any line wider than the terminal at a space, for
//! text from other pieces and for narrower terminals.

/// The widest a hand-wrapped line of help prose may be, indent included.
/// The tests hold every string here and in `cli.rs` to it.
#[cfg_attr(not(test), allow(dead_code))]
pub const PROSE_WIDTH: usize = 78;

pub const ROOT_ABOUT: &str = "Work with an Asana workspace from the command line.";

/// The root screen's examples open with the first-run path, since nothing
/// works before a token is stored: the one command that logs in, with the
/// page the token comes from in its comment. Then one taste of each noun,
/// as gh's root screen gives one of `issue`, `repo` and `pr`, none of them
/// repeating the group screens' own examples. gh's leaf screens group
/// examples this way, a `# comment` line over each block and a blank line
/// between.
pub const ROOT_EXAMPLES: &str = "\
# Log in with a personal access token from https://app.asana.com/0/my-apps
$ asana auth login

# Work with tasks and projects
$ asana task list
$ asana project view \"Roadmap\"
$ asana task create --title \"Fix the login page\" --project \"Roadmap\"";

/// The `auth:` line on the root screen and the first line of `asana auth
/// --help` say the same thing, as gh's do ("Authenticate gh and git with
/// GitHub" on both).
pub const AUTH_ABOUT: &str = "Authenticate with Asana";

/// `asana auth --help` is where a new user looks for how to log in, so its
/// description says how, in the order gh's `auth login` screen and its
/// no-auth error use: `asana auth login` first (what it asks for, where the
/// token comes from, where it is stored), the environment variable second
/// as the route for scripts, then the command that checks the result.
/// `AUTH_ABOUT` plus a period, then the how-to, hand-wrapped like gh's
/// heredoc descriptions.
pub const AUTH_LONG_ABOUT: &str = "\
Authenticate with Asana.

Run `asana auth login` to get started: it prompts for a personal access
token from https://app.asana.com/0/my-apps, checks it against Asana and
stores it in ~/.config/asana/config.toml for every other command to use.
Alternatively, populate the ASANA_TOKEN environment variable with a token,
which suits scripts and automation; `asana help environment` has the
details. Run `asana auth status` to see which account is in use and where
its token came from.";

/// The auth group's examples, commented like `gh auth login --help`'s: the
/// interactive login, the scripted one, and the check.
pub const AUTH_EXAMPLES: &str = "\
# Log in by pasting a token when prompted
$ asana auth login

# Log in from a script by reading the token from a file
$ asana auth login --with-token < token.txt

# Check which account and workspace are in use
$ asana auth status";

pub const MANUAL_URL: &str = "https://github.com/davediv/asana-cli#readme";

pub const RELEASES_URL: &str = "https://github.com/davediv/asana-cli/releases/tag";

/// The `LEARN MORE` footer printed on every help screen.
pub fn learn_more() -> Vec<String> {
    vec![
        "Use `asana <command> <subcommand> --help` for more information on a command.".to_string(),
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

/// How the subcommands of a command divide by purpose, gh's `GENERAL
/// COMMANDS` (act on the collection: `create`, `list`) against `TARGETED
/// COMMANDS` (act on one thing named as an argument). Commands not named
/// here fall into `ADDITIONAL COMMANDS`. This is the classification only:
/// the renderer in `cli.rs` prints the split when every section holds at
/// least two commands (`task` does) and otherwise one `AVAILABLE COMMANDS`
/// section, so `project`, with one of each, reads like `gh auth --help`
/// rather than like a template built for a fifteen-command group.
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
        body: Body::Text(
            "`ASANA_TOKEN`, `ASANA_ACCESS_TOKEN` (in order of precedence): a personal\n\
               access token used to authenticate with Asana. Setting either overrides\n\
               `token` in the config file. Create one at https://app.asana.com/0/my-apps.\n\
               \n\
               `ASANA_WORKSPACE`: the gid or name of the workspace commands operate in.\n\
               Overrides `workspace` in the config file; the `--workspace` flag overrides\n\
               both. When nothing is set and the token belongs to exactly one workspace,\n\
               that workspace is used.\n\
               \n\
               `XDG_CONFIG_HOME`: the directory the config file is read from, as\n\
               `asana/config.toml` under it. If not set, `$HOME/.config/asana/config.toml`\n\
               is used. The file may hold `token = \"...\"` and `workspace = \"...\"`.\n\
               \n\
               `ASANA_BASE_URL`: the API base URL to use instead of\n\
               `https://app.asana.com/api/1.0`, for testing against a mock server.\n\
               \n\
               `NO_COLOR`: set to any value to avoid printing ANSI escape sequences for\n\
               color output.\n\
               \n\
               `TERM`: set to `dumb` to disable color output.\n\
               \n\
               `COLUMNS`: the width to lay tables out for when the terminal size cannot be\n\
               detected (default 80).\n",
        ),
        example: "",
    },
    Topic {
        name: "exit-codes",
        short: "Exit codes used by asana",
        body: Body::Text(
            "asana follows normal conventions regarding exit codes.\n\
               \n\
               - If a command completes successfully, the exit code will be 0\n\
               \n\
               - If a command fails for any reason, the exit code will be 1\n\
               \n\
               - If a command is running but gets cancelled, the exit code will be 2\n\
               \n\
               - If a command requires authentication, the exit code will be 4\n\
               \n\
               NOTE: Usage errors, such as an unknown flag or a missing argument, exit\n\
               with 1 like any other failure. Completing a task that is already complete\n\
               counts as success and exits with 0.\n",
        ),
        example: "",
    },
    Topic {
        name: "formatting",
        short: "Formatting options for JSON and piped output from asana",
        body: Body::Text(
            "By default, the result of `asana` commands is output in line-based plain\n\
               text format: lists as tables and views as a labelled summary, laid out for\n\
               the width of the terminal and colored when stdout is a terminal.\n\
               \n\
               When stdout is not a terminal (a pipe, a file, a `$(...)` substitution), the\n\
               same commands print text meant for other programs instead. Lists are\n\
               tab-separated with no header, one item per line, with ISO-8601 timestamps;\n\
               views are `key:<TAB>value` lines, then a line holding `--`, then the body as\n\
               written. Nothing is colored or truncated, so `cut`, `awk` and `grep` work on\n\
               the output as is. Pass `--no-color` or set `NO_COLOR` to drop color on a\n\
               terminal as well.\n\
               \n\
               Commands that read from Asana (`list` and `view`) also support the `--json`\n\
               flag, which prints the objects the API returned instead: an array for\n\
               lists, one object for views, pretty-printed and never colored. The flag\n\
               takes no field list; every field the command fetched is included, so pipe\n\
               the output into `jq` to select or reshape it. To learn about jq query\n\
               syntax, see: https://jqlang.github.io/jq/manual/\n\
               \n\
               Commands that create something (`task create`, `task comment`) print its\n\
               permalink URL alone on stdout, and every command's progress and status\n\
               lines (`\u{2713} Completed task ...`) go to stderr, so `$(asana task create ...)`\n\
               captures the URL and nothing else.\n",
        ),
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

    /// Prose is hand-wrapped short of `PROSE_WIDTH` (indent included, so a
    /// titled section's body has two columns less), URLs are bare, and no
    /// line ends in a space.
    fn assert_prose(what: &str, text: &str, indent: usize) {
        for line in text.lines() {
            assert!(
                line.chars().count() + indent <= PROSE_WIDTH,
                "{what}: line wider than {PROSE_WIDTH} columns: {line}"
            );
            assert!(line.is_empty() || !line.ends_with(' '), "{what}: {line:?}");
            assert!(!line.contains("<http"), "{what}: bracketed URL: {line}");
        }
    }

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
            assert_prose(t.name, long, 0);
            // Examples are printed verbatim under a two-space indent, so a
            // command or a row of sample output may run to 80 columns.
            for line in t.example.lines() {
                assert!(line.len() + 2 <= 80, "{}: example too wide: {line}", t.name);
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

    /// The auth prose and the root examples follow the same rules as the
    /// topics: a summary with no period, a description that opens with that
    /// summary and a period, lines short of 78 columns, bare URLs, no
    /// trailing space, and examples made of `$ ` invocations, `# ` comments
    /// and blank lines.
    #[test]
    fn auth_prose_and_root_examples_are_well_formed() {
        assert!(!AUTH_ABOUT.ends_with('.'));
        assert!(AUTH_LONG_ABOUT.starts_with(&format!("{AUTH_ABOUT}.\n\n")));
        assert!(!AUTH_LONG_ABOUT.ends_with('\n'));
        // Login leads, the environment variable is the alternative, and
        // the check comes last, as on gh's login screen.
        let login = AUTH_LONG_ABOUT.find("`asana auth login`").expect("login");
        let env = AUTH_LONG_ABOUT
            .find("Alternatively, populate the ASANA_TOKEN environment variable")
            .expect("env var");
        let status = AUTH_LONG_ABOUT.find("`asana auth status`").expect("status");
        assert!(login < env && env < status);
        assert!(AUTH_LONG_ABOUT.contains("https://app.asana.com/0/my-apps,"));
        assert!(AUTH_LONG_ABOUT.contains("~/.config/asana/config.toml"));
        assert!(AUTH_LONG_ABOUT.contains("`asana help environment`"));
        assert!(!AUTH_LONG_ABOUT.contains("export"));
        assert_prose("auth", AUTH_LONG_ABOUT, 0);
        assert_prose("root about", ROOT_ABOUT, 0);
        for ex in [ROOT_EXAMPLES, AUTH_EXAMPLES] {
            assert!(ex.contains("\n$ asana auth login\n"));
            assert!(
                !ex.contains("ASANA_TOKEN"),
                "examples are commands, not exports"
            );
            assert!(ex.starts_with("# "));
            assert!(!ex.ends_with('\n'));
            assert_prose("examples", ex, 2);
            for line in ex.lines() {
                assert!(
                    line.is_empty() || line.starts_with("$ ") || line.starts_with("# "),
                    "not an example line: {line:?}"
                );
            }
        }
        assert!(ROOT_EXAMPLES.contains("https://app.asana.com/0/my-apps\n$ asana auth login\n"));
        assert!(AUTH_EXAMPLES.contains("\n$ asana auth login --with-token < token.txt\n"));
        assert!(AUTH_EXAMPLES.ends_with("\n$ asana auth status"));
        for path in [&[][..], &["task"], &["project"]] {
            if let Some(text) = arguments(path) {
                assert_prose("arguments", text, 2);
            }
        }
    }

    /// The footer fits an 80-column terminal under its indent and points at
    /// every prose topic.
    #[test]
    fn learn_more_points_at_the_topics() {
        let lines = learn_more();
        assert_eq!(lines.len(), 5);
        assert_prose("learn more", &lines.join("\n"), 2);
        assert_eq!(lines[1], format!("Read the manual at {MANUAL_URL}"));
        for t in TOPICS.iter().filter(|t| matches!(t.body, Body::Text(_))) {
            assert!(lines
                .iter()
                .any(|l| l.contains(&format!("asana help {}", t.name))));
        }
    }
}
