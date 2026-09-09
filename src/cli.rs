//! The clap command tree, global flags, and the gh-style help renderer.
//!
//! clap does the parsing; the help screens and usage errors are rendered here
//! from the built `Command` tree so they read exactly like gh's.

use std::error::Error as _;
use std::ffi::OsString;
use std::fmt;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{ArgAction, Command, CommandFactory, Parser, Subcommand};

use crate::cmd::{
    auth_status, help, project_list, project_view, task_assign, task_comment, task_complete,
    task_create, task_delete, task_list, task_view, workspace_list,
};
use crate::render::Palette;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "asana",
    bin_name = "asana",
    version = VERSION,
    about = help::ROOT_ABOUT,
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true,
    after_help = help::ROOT_EXAMPLES
)]
pub struct Cli {
    /// Select the workspace by name or gid
    #[arg(long, global = true, value_name = "name")]
    pub workspace: Option<String>,

    /// Disable color output
    #[arg(long, global = true, action = ArgAction::SetTrue)]
    pub no_color: bool,

    /// Show help for command
    #[arg(long, short = 'h', global = true, action = ArgAction::Help)]
    help: Option<bool>,

    /// Show asana version
    #[arg(long, action = ArgAction::Version)]
    version: Option<bool>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(
        subcommand,
        about = "Check authentication with Asana",
        long_about = "Check which Asana account and workspace the CLI is using.",
        after_help = "$ asana auth status"
    )]
    Auth(AuthCmd),
    #[command(
        subcommand,
        about = "Manage workspaces",
        long_about = "Work with the Asana workspaces your token can reach.",
        after_help = "$ asana workspace list"
    )]
    Workspace(WorkspaceCmd),
    #[command(
        subcommand,
        about = "Manage projects",
        long_about = "Work with Asana projects.",
        after_help = "$ asana project list\n$ asana project view \"Roadmap\"\n$ asana project view 1200000000000000 --web"
    )]
    Project(ProjectCmd),
    #[command(
        subcommand,
        about = "Manage tasks",
        long_about = "Work with Asana tasks.",
        after_help = "$ asana task list\n$ asana task create --title \"Fix the login page\"\n$ asana task view 1211234567890123 --web"
    )]
    Task(TaskCmd),
}

#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    Status(auth_status::Args),
}

#[derive(Subcommand, Debug)]
pub enum WorkspaceCmd {
    #[command(visible_alias = "ls")]
    List(workspace_list::Args),
}

#[derive(Subcommand, Debug)]
pub enum ProjectCmd {
    #[command(visible_alias = "ls")]
    List(project_list::Args),
    View(project_view::Args),
}

#[derive(Subcommand, Debug)]
pub enum TaskCmd {
    #[command(visible_alias = "ls")]
    List(task_list::Args),
    View(task_view::Args),
    #[command(visible_alias = "new")]
    Create(task_create::Args),
    #[command(visible_alias = "close")]
    Complete(task_complete::CompleteArgs),
    Reopen(task_complete::ReopenArgs),
    Assign(task_assign::Args),
    Comment(task_comment::Args),
    Delete(task_delete::Args),
}

impl Commands {
    /// Checks that need neither a token nor the network, so bad identifiers
    /// fail fast like gh's `invalid issue format: "abc"`.
    pub fn prevalidate(&self) -> anyhow::Result<()> {
        use crate::resolve::parse_task_id;
        let check_all =
            |tasks: &[String]| tasks.iter().try_for_each(|t| parse_task_id(t).map(drop));
        match self {
            Commands::Task(TaskCmd::View(a)) => parse_task_id(&a.task).map(drop),
            Commands::Task(TaskCmd::Assign(a)) => parse_task_id(&a.task).map(drop),
            Commands::Task(TaskCmd::Comment(a)) => parse_task_id(&a.task).map(drop),
            Commands::Task(TaskCmd::Delete(a)) => parse_task_id(&a.task).map(drop),
            Commands::Task(TaskCmd::Complete(a)) => check_all(&a.tasks),
            Commands::Task(TaskCmd::Reopen(a)) => check_all(&a.tasks),
            Commands::Task(TaskCmd::Create(a)) => a
                .due
                .as_deref()
                .map(|d| task_create::parse_due(d, crate::render::time::today()))
                .transpose()
                .map(drop),
            Commands::Task(TaskCmd::List(a)) if a.limit == 0 => {
                anyhow::bail!("invalid limit: {}", a.limit)
            }
            Commands::Project(ProjectCmd::List(a)) if a.limit == 0 => {
                anyhow::bail!("invalid limit: {}", a.limit)
            }
            _ => Ok(()),
        }
    }

    /// Whether the invoked leaf asked for `--json`.
    pub fn wants_json(&self) -> bool {
        match self {
            Commands::Project(ProjectCmd::List(a)) => a.json,
            Commands::Project(ProjectCmd::View(a)) => a.json,
            Commands::Task(TaskCmd::List(a)) => a.json,
            Commands::Task(TaskCmd::View(a)) => a.json,
            _ => false,
        }
    }
}

/// A usage error raised at runtime (e.g. a missing `--title` when not
/// interactive). `main` prints the message, a blank line, then the usage and
/// flag table of the command, and exits 1, exactly like gh.
#[derive(Debug)]
pub struct UsageError {
    pub message: String,
    pub path: Vec<String>,
}

impl UsageError {
    pub fn new(path: &[&str], message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: path.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UsageError {}

/// Everything has already been printed; just exit with this code.
#[derive(Debug)]
pub struct SilentExit(pub i32);

impl fmt::Display for SilentExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exit {}", self.0)
    }
}

impl std::error::Error for SilentExit {}

/// The user cancelled a prompt. Exit code 2.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// The fully built command tree (globals propagated into subcommands).
pub fn command() -> Command {
    let mut cmd = Cli::command();
    cmd.build();
    cmd
}

fn find<'a>(root: &'a Command, path: &[String]) -> Option<&'a Command> {
    let mut cur = root;
    for name in path {
        cur = cur.find_subcommand(name)?;
    }
    Some(cur)
}

/// The bare words of argv (flags skipped, along with `--workspace`'s value),
/// up to `--`.
fn bare_words(argv: &[OsString]) -> Vec<String> {
    let mut words = Vec::new();
    let mut iter = argv.iter().skip(1);
    while let Some(a) = iter.next() {
        let s = a.to_string_lossy();
        if s == "--" {
            break;
        }
        if s.starts_with('-') {
            if s == "--workspace" {
                iter.next();
            }
            continue;
        }
        words.push(s.into_owned());
    }
    words
}

/// A direct subcommand by name or alias.
fn subcommand<'a>(cmd: &'a Command, name: &str) -> Option<&'a Command> {
    cmd.get_subcommands()
        .find(|c| c.get_name() == name || c.get_all_aliases().any(|al| al == name))
}

/// Work out which command the user was addressing from the raw argv, so help
/// and usage errors are rendered for the right level.
pub fn command_path(root: &Command, argv: &[OsString]) -> Vec<String> {
    let mut path = Vec::new();
    let mut cur = root;
    for word in bare_words(argv) {
        if word == "help" {
            continue;
        }
        match subcommand(cur, &word) {
            Some(sub) => {
                path.push(sub.get_name().to_string());
                cur = sub;
            }
            None => break,
        }
    }
    path
}

/// `asana help [<command>... | <topic>]` and `asana version`, which gh
/// supports and clap reports as unknown subcommands. `help` follows the
/// command tree as far as it matches, like cobra's; a first word that is
/// neither a command nor a topic is `Unknown help topic`.
fn builtin_request(root: &Command, argv: &[OsString], palette: &Palette) -> Option<Rendered> {
    let ok = |stdout: String| Rendered {
        stdout,
        stderr: String::new(),
        code: 0,
    };
    let words = bare_words(argv);
    match words.first().map(String::as_str) {
        Some("version") => Some(ok(render_version())),
        Some("help") => {
            let rest = &words[1..];
            let Some(first) = rest.first() else {
                return Some(ok(render_help(&[], palette)));
            };
            if let Some(topic) = help::topic(first) {
                return Some(ok(render_topic(topic)));
            }
            let mut path = Vec::new();
            let mut cur = root;
            for word in rest {
                match subcommand(cur, word) {
                    Some(sub) => {
                        path.push(sub.get_name().to_string());
                        cur = sub;
                    }
                    None => break,
                }
            }
            if path.is_empty() {
                return Some(Rendered {
                    stdout: String::new(),
                    stderr: format!("Unknown help topic [`{first}`]\n{}", usage_block(&[])),
                    code: 1,
                });
            }
            Some(ok(render_help(&path, palette)))
        }
        _ => None,
    }
}

fn strs(path: &[String]) -> Vec<&str> {
    path.iter().map(String::as_str).collect()
}

fn usage_line(path: &[String]) -> String {
    let p = strs(path);
    if let Some(u) = help::usage(&p) {
        return u.to_string();
    }
    let cmd = command();
    let leaf = find(&cmd, path);
    let has_subs = leaf.is_some_and(|c| c.get_subcommands().next().is_some());
    let mut s = String::from("asana");
    for part in path {
        s.push(' ');
        s.push_str(part);
    }
    if has_subs {
        s.push_str(" <command>");
    }
    s.push_str(" [flags]");
    s
}

/// One flag as cobra prints it: `-s, --long placeholder` and its description.
struct FlagLine {
    left: String,
    help: String,
}

fn flag_lines(args: Vec<&clap::Arg>) -> Vec<FlagLine> {
    let mut args = args;
    args.sort_by_key(|a| a.get_long().unwrap_or("").to_string());
    // gh lists `--help` without its `-h` shorthand.
    let short_of = |a: &clap::Arg| a.get_short().filter(|_| a.get_long() != Some("help"));
    let any_short = args.iter().any(|a| short_of(a).is_some());
    args.iter()
        .map(|a| {
            let mut left = String::new();
            match (short_of(a), any_short) {
                (Some(s), _) => left.push_str(&format!("-{s}, ")),
                (None, true) => left.push_str("    "),
                (None, false) => {}
            }
            left.push_str(&format!("--{}", a.get_long().unwrap_or("")));
            let takes_value = a.get_num_args().is_some_and(|n| n.takes_values());
            if takes_value {
                let name = a
                    .get_value_names()
                    .and_then(|v| v.first())
                    .map(|v| v.to_string().to_lowercase())
                    .unwrap_or_else(|| "string".to_string());
                left.push(' ');
                left.push_str(&name);
            }
            let mut help = a.get_help().map(|h| h.to_string()).unwrap_or_default();
            if let Some(default) = a.get_default_values().first() {
                let d = default.to_string_lossy();
                if d.parse::<f64>().is_ok() {
                    help.push_str(&format!(" (default {d})"));
                } else if !d.is_empty() && d != "false" {
                    help.push_str(&format!(" (default \"{d}\")"));
                }
            }
            FlagLine { left, help }
        })
        .collect()
}

fn render_flag_table(lines: &[FlagLine]) -> String {
    let max = lines
        .iter()
        .map(|l| l.left.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for l in lines {
        let pad = " ".repeat(max - l.left.chars().count());
        out.push_str(&format!("  {}{}   {}\n", l.left, pad, l.help));
    }
    out
}

fn local_flags(cmd: &Command) -> Vec<&clap::Arg> {
    cmd.get_arguments()
        .filter(|a| !a.is_positional() && !a.is_hide_set() && !a.is_global_set())
        .collect()
}

fn inherited_flags(cmd: &Command) -> Vec<&clap::Arg> {
    cmd.get_arguments()
        .filter(|a| !a.is_positional() && !a.is_hide_set() && a.is_global_set())
        .collect()
}

fn root_flags(cmd: &Command) -> Vec<&clap::Arg> {
    cmd.get_arguments()
        .filter(|a| !a.is_positional() && !a.is_hide_set())
        .collect()
}

/// The `Usage:` + `Flags:` block cobra prints under usage errors.
pub fn usage_block(path: &[String]) -> String {
    let root = command();
    let mut out = format!("Usage:  {}\n", usage_line(path));
    if let Some(cmd) = find(&root, path) {
        if cmd.get_subcommands().next().is_some() {
            out.push_str("\nAvailable commands:\n");
            let mut names: Vec<&str> = cmd
                .get_subcommands()
                .filter(|c| !c.is_hide_set())
                .map(Command::get_name)
                .collect();
            names.sort_unstable();
            for n in names {
                out.push_str(&format!("  {n}\n"));
            }
        } else {
            let flags = if path.is_empty() {
                root_flags(cmd)
            } else {
                local_flags(cmd)
            };
            let lines = flag_lines(flags);
            if !lines.is_empty() {
                out.push_str("\nFlags:\n");
                out.push_str(&render_flag_table(&lines));
                out.push_str("  \n");
            }
        }
    }
    out
}

/// `asana version` / `asana --version`, two lines like gh's.
pub fn render_version() -> String {
    format!(
        "asana version {VERSION}\n{}/v{VERSION}\n",
        help::RELEASES_URL
    )
}

/// `asana help <topic>`: the topic text alone, like gh's help topics.
pub fn render_topic(topic: &help::Topic) -> String {
    topic.long.to_string()
}

/// Render the gh-shaped help screen for a command path.
pub fn render_help(path: &[String], palette: &Palette) -> String {
    let root = command();
    let Some(cmd) = find(&root, path) else {
        return render_help(&[], palette);
    };
    let heading = |s: &str| help::heading(palette, s);
    let mut out = String::new();

    let about = if path.is_empty() {
        help::ROOT_ABOUT.to_string()
    } else {
        cmd.get_long_about()
            .or(cmd.get_about())
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    out.push_str(about.trim_end());
    out.push_str("\n\n");

    out.push_str(&heading("USAGE"));
    out.push_str(&format!("\n  {}\n\n", usage_line(path)));

    let aliases: Vec<&str> = cmd.get_visible_aliases().collect();
    if !aliases.is_empty() {
        out.push_str(&heading("ALIASES"));
        out.push('\n');
        let parent = path[..path.len().saturating_sub(1)].join(" ");
        for a in aliases {
            if parent.is_empty() {
                out.push_str(&format!("  asana {a}\n"));
            } else {
                out.push_str(&format!("  asana {parent} {a}\n"));
            }
        }
        out.push('\n');
    }

    let subs: Vec<&Command> = cmd.get_subcommands().filter(|c| !c.is_hide_set()).collect();
    if !subs.is_empty() {
        // cobra pads `name:` to the longest name plus one, never under 15.
        let topic_names = help::TOPICS
            .iter()
            .filter(|_| path.is_empty())
            .map(|t| t.name);
        let pad = subs
            .iter()
            .map(|c| c.get_name())
            .chain(topic_names)
            .map(|n| n.len() + 2)
            .max()
            .unwrap_or(0)
            .max(15);
        let groups = help::groups(&strs(path));
        let mut placed: Vec<&str> = Vec::new();
        let mut sections: Vec<(&str, Vec<&Command>)> = Vec::new();
        for (title, names) in &groups {
            let mut members = Vec::new();
            for n in names.iter() {
                if let Some(c) = subs.iter().find(|c| c.get_name() == *n) {
                    members.push(*c);
                    placed.push(n);
                }
            }
            if !members.is_empty() {
                sections.push((title, members));
            }
        }
        let mut rest: Vec<&Command> = subs
            .iter()
            .filter(|c| !placed.contains(&c.get_name()))
            .copied()
            .collect();
        rest.sort_by_key(|c| c.get_name().to_string());
        if !rest.is_empty() {
            let title = if sections.is_empty() {
                "COMMANDS"
            } else {
                "ADDITIONAL COMMANDS"
            };
            sections.push((title, rest));
        }
        for (title, members) in sections {
            out.push_str(&heading(title));
            out.push('\n');
            for c in members {
                let about = c.get_about().map(|s| s.to_string()).unwrap_or_default();
                out.push_str(&format!(
                    "  {:<pad$}{}\n",
                    format!("{}:", c.get_name()),
                    about,
                    pad = pad
                ));
            }
            out.push('\n');
        }
        if path.is_empty() {
            out.push_str(&heading("HELP TOPICS"));
            out.push('\n');
            for t in help::TOPICS {
                out.push_str(&format!(
                    "  {:<pad$}{}\n",
                    format!("{}:", t.name),
                    t.short,
                    pad = pad
                ));
            }
            out.push('\n');
        }
    }

    let (local, inherited) = if path.is_empty() {
        (root_flags(cmd), Vec::new())
    } else {
        (local_flags(cmd), inherited_flags(cmd))
    };
    let local_lines = flag_lines(local);
    if !local_lines.is_empty() {
        out.push_str(&heading("FLAGS"));
        out.push('\n');
        out.push_str(&render_flag_table(&local_lines));
        out.push('\n');
    }
    let inherited_lines = flag_lines(inherited);
    if !inherited_lines.is_empty() {
        out.push_str(&heading("INHERITED FLAGS"));
        out.push('\n');
        out.push_str(&render_flag_table(&inherited_lines));
        out.push('\n');
    }

    if let Some(text) = help::arguments(&strs(path)) {
        out.push_str(&heading("ARGUMENTS"));
        out.push('\n');
        for line in text.lines() {
            out.push_str(&format!("  {line}\n"));
        }
        out.push('\n');
    }

    let examples = if path.is_empty() {
        Some(help::ROOT_EXAMPLES.to_string())
    } else {
        cmd.get_after_help().map(|s| s.to_string())
    };
    if let Some(ex) = examples.filter(|e| !e.trim().is_empty()) {
        out.push_str(&heading("EXAMPLES"));
        out.push('\n');
        for line in ex.trim_end().lines() {
            if line.is_empty() {
                out.push_str("  \n");
            } else {
                out.push_str(&format!("  {line}\n"));
            }
        }
        out.push('\n');
    }

    out.push_str(&heading("LEARN MORE"));
    out.push('\n');
    for line in help::learn_more() {
        out.push_str(&format!("  {line}\n"));
    }
    out.push('\n');
    out
}

fn context_string(err: &clap::Error, kind: ContextKind) -> Option<String> {
    match err.get(kind) {
        Some(ContextValue::String(s)) => Some(s.clone()),
        Some(ContextValue::Strings(v)) => v.first().cloned(),
        Some(ContextValue::StyledStr(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn context_strings(err: &clap::Error, kind: ContextKind) -> Vec<String> {
    match err.get(kind) {
        Some(ContextValue::Strings(v)) => v.clone(),
        Some(ContextValue::String(s)) => vec![s.clone()],
        Some(ContextValue::StyledStrs(v)) => v.iter().map(|s| s.to_string()).collect(),
        Some(ContextValue::StyledStr(s)) => vec![s.to_string()],
        _ => Vec::new(),
    }
}

/// `--title <string>` -> `--title`; `<task>` stays as is.
fn flag_name(s: &str) -> String {
    s.split_whitespace().next().unwrap_or(s).to_string()
}

fn count_positionals(cmd: &Command) -> (usize, bool) {
    let positionals: Vec<&clap::Arg> = cmd.get_positionals().collect();
    let variadic = positionals
        .iter()
        .any(|p| p.get_num_args().is_some_and(|n| n.max_values() > 1));
    (positionals.len(), variadic)
}

fn suggestions(typed: &str, names: &[String]) -> Vec<String> {
    let t = typed.to_lowercase();
    let mut out: Vec<String> = names
        .iter()
        .filter(|n| {
            let l = n.to_lowercase();
            strsim::levenshtein(&t, &l) <= 2 || l.starts_with(&t)
        })
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What to print (stdout, stderr) and the exit code for a clap error.
pub struct Rendered {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// `unknown command "lsit" for "asana task"` + suggestions + the command list.
fn unknown_command(typed: &str, root: &Command, path: &[String]) -> Rendered {
    let parent = std::iter::once("asana".to_string())
        .chain(path.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut msg = format!("unknown command \"{typed}\" for \"{parent}\"\n");
    let names: Vec<String> = find(root, path)
        .map(|c| {
            c.get_subcommands()
                .filter(|s| !s.is_hide_set())
                .map(|s| s.get_name().to_string())
                .collect()
        })
        .unwrap_or_default();
    let suggested = suggestions(typed, &names);
    if !suggested.is_empty() {
        msg.push_str("\nDid you mean this?\n");
        for s in suggested {
            msg.push_str(&format!("\t{s}\n"));
        }
    }
    Rendered {
        stdout: String::new(),
        stderr: format!("{msg}\n{}", usage_block(path)),
        code: 1,
    }
}

/// Map a clap error to gh's wording. Help and version go to stdout with exit
/// 0; usage errors go to stderr with exit 1.
pub fn render_clap_error(err: &clap::Error, argv: &[OsString], palette: &Palette) -> Rendered {
    let root = command();
    let path = command_path(&root, argv);
    let ok = |stdout: String| Rendered {
        stdout,
        stderr: String::new(),
        code: 0,
    };
    let usage = |message: String, path: &[String]| Rendered {
        stdout: String::new(),
        stderr: format!("{message}\n\n{}", usage_block(path)),
        code: 1,
    };
    let bare = |message: String| Rendered {
        stdout: String::new(),
        stderr: format!("{message}\n"),
        code: 1,
    };
    if matches!(
        err.kind(),
        ErrorKind::InvalidSubcommand | ErrorKind::UnknownArgument
    ) {
        if let Some(rendered) = builtin_request(&root, argv, palette) {
            return rendered;
        }
    }
    match err.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        | ErrorKind::MissingSubcommand => ok(render_help(&path, palette)),
        ErrorKind::DisplayVersion => ok(render_version()),
        ErrorKind::InvalidSubcommand => unknown_command(
            &context_string(err, ContextKind::InvalidSubcommand).unwrap_or_default(),
            &root,
            &path,
        ),
        ErrorKind::UnknownArgument => {
            let arg = context_string(err, ContextKind::InvalidArg).unwrap_or_default();
            let leaf = find(&root, &path);
            let is_group = leaf.is_some_and(|c| c.get_subcommands().next().is_some());
            if arg.starts_with('-') {
                usage(format!("unknown flag: {}", flag_name(&arg)), &path)
            } else if is_group {
                unknown_command(&arg, &root, &path)
            } else {
                let (n, variadic) = leaf.map(count_positionals).unwrap_or((0, false));
                if variadic {
                    bare(format!("unexpected argument \"{arg}\""))
                } else {
                    bare(format!("accepts {n} arg(s), received {}", n + 1))
                }
            }
        }
        ErrorKind::MissingRequiredArgument => {
            let missing = context_strings(err, ContextKind::InvalidArg);
            let leaf = find(&root, &path);
            let (n, variadic) = leaf.map(count_positionals).unwrap_or((0, false));
            let missing_positionals = missing.iter().filter(|m| !m.starts_with('-')).count();
            if missing_positionals > 0 {
                let received = n.saturating_sub(missing_positionals);
                if variadic {
                    bare(format!(
                        "requires at least 1 arg(s), only received {received}"
                    ))
                } else {
                    bare(format!("accepts {n} arg(s), received {received}"))
                }
            } else {
                let flags: Vec<String> = missing
                    .iter()
                    .map(|m| format!("`{}`", flag_name(m)))
                    .collect();
                usage(
                    format!("required flag(s) {} not set", flags.join(", ")),
                    &path,
                )
            }
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            let arg = flag_name(&context_string(err, ContextKind::InvalidArg).unwrap_or_default());
            let value = context_string(err, ContextKind::InvalidValue).unwrap_or_default();
            let valid = context_strings(err, ContextKind::ValidValue);
            if value.is_empty() && err.kind() == ErrorKind::InvalidValue {
                usage(format!("flag needs an argument: {arg}"), &path)
            } else if !valid.is_empty() {
                usage(
                    format!(
                        "invalid argument \"{value}\" for \"{arg}\" flag: must be one of {}",
                        valid.join(", ")
                    ),
                    &path,
                )
            } else {
                let why = err
                    .source()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "invalid syntax".to_string());
                usage(
                    format!("invalid argument \"{value}\" for \"{arg}\" flag: {why}"),
                    &path,
                )
            }
        }
        ErrorKind::ArgumentConflict => {
            let a = flag_name(&context_string(err, ContextKind::InvalidArg).unwrap_or_default());
            let others = context_strings(err, ContextKind::PriorArg);
            let b = others.first().map(|s| flag_name(s)).unwrap_or_default();
            let (first, second) = if a <= b { (a, b) } else { (b, a) };
            usage(
                format!("specify only one of `{first}` or `{second}`"),
                &path,
            )
        }
        ErrorKind::TooManyValues | ErrorKind::TooFewValues | ErrorKind::WrongNumberOfValues => {
            let (n, _) = find(&root, &path)
                .map(count_positionals)
                .unwrap_or((0, false));
            let got = context_string(err, ContextKind::ActualNumValues).unwrap_or_default();
            bare(format!("accepts {n} arg(s), received {got}"))
        }
        _ => {
            let text = err.to_string();
            let first = text
                .lines()
                .next()
                .unwrap_or("")
                .trim_start_matches("error: ")
                .to_string();
            usage(first, &path)
        }
    }
}

/// Shared helper for command modules: flatten `Args` into a usage-error.
pub fn usage_error(path: &[&str], message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError::new(path, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn command_path_follows_subcommands_and_aliases() {
        let root = command();
        assert_eq!(
            command_path(&root, &argv(&["asana", "task", "ls", "--help"])),
            vec!["task", "list"]
        );
        assert_eq!(
            command_path(&root, &argv(&["asana", "--workspace", "x", "task", "lsit"])),
            vec!["task"]
        );
        assert_eq!(
            command_path(&root, &argv(&["asana", "help", "project"])),
            vec!["project"]
        );
        assert!(command_path(&root, &argv(&["asana", "--help"])).is_empty());
    }

    #[test]
    fn help_screens_have_gh_sections() {
        let p = Palette::off();
        let root = render_help(&[], &p);
        assert!(root.starts_with(help::ROOT_ABOUT));
        for section in ["USAGE", "CORE COMMANDS", "FLAGS", "EXAMPLES", "LEARN MORE"] {
            assert!(
                root.contains(&format!("\n{section}\n")),
                "{section} missing in:\n{root}"
            );
        }
        let task = render_help(&["task".to_string()], &p);
        assert!(task.contains("\nGENERAL COMMANDS\n"));
        assert!(task.contains("\nTARGETED COMMANDS\n"));
        assert!(task.contains("\nARGUMENTS\n"));
        assert!(task.contains("  asana task <command> [flags]"));
        let list = render_help(&["task".to_string(), "list".to_string()], &p);
        assert!(list.contains("\nALIASES\n  asana task ls\n"));
        assert!(list.contains("-L, --limit int"));
        assert!(list.contains("(default 30)"));
        assert!(list.contains("\nINHERITED FLAGS\n"));
    }

    /// The root screen, character for character: every string on it belongs
    /// to this piece, so it can be pinned. Its shape is gh's `gh --help`.
    #[test]
    fn root_help_reads_like_gh() {
        let expected = "\
Work with an Asana workspace from the command line.

USAGE
  asana <command> <subcommand> [flags]

CORE COMMANDS
  auth:          Check authentication with Asana
  project:       Manage projects
  task:          Manage tasks
  workspace:     Manage workspaces

HELP TOPICS
  environment:   Environment variables that can be used with asana
  exit-codes:    Exit codes used by asana

FLAGS
  --help             Show help for command
  --no-color         Disable color output
  --version          Show asana version
  --workspace name   Select the workspace by name or gid

EXAMPLES
  $ asana task list
  $ asana task create --title \"Fix the login page\"
  $ asana task view 1211234567890123 --web

LEARN MORE
  Use `asana <command> <subcommand> --help` for more information about a command.
  Read the manual at https://github.com/davediv/asana-cli#readme
  Learn about exit codes using `asana help exit-codes`
  Learn about environment variables using `asana help environment`

";
        assert_eq!(render_help(&[], &Palette::off()), expected);
    }

    #[test]
    fn headings_use_gh_colour_bytes_only_when_enabled() {
        let coloured = render_help(&[], &Palette::new(true));
        assert!(coloured.contains("\x1b[0;1;39mUSAGE\x1b[0m\n"));
        assert!(coloured.contains("\x1b[0;1;39mLEARN MORE\x1b[0m\n"));
        // Nothing but the headings is styled.
        let stripped = coloured.replace("\x1b[0;1;39m", "").replace("\x1b[0m", "");
        assert_eq!(stripped, render_help(&[], &Palette::off()));
        assert!(!render_help(&[], &Palette::off()).contains('\x1b'));
    }

    #[test]
    fn group_help_lists_inherited_flags_and_arguments() {
        let p = Palette::off();
        let project = render_help(&["project".to_string()], &p);
        let expected_tail = "\
INHERITED FLAGS
  --help             Show help for command
  --no-color         Disable color output
  --workspace name   Select the workspace by name or gid

ARGUMENTS
  A project can be supplied as argument in any of the following formats:
";
        assert!(project.contains(expected_tail), "{project}");
        assert!(!project.contains("\nFLAGS\n"));
        assert!(!project.contains("HELP TOPICS"));
        assert!(project.ends_with("`asana help environment`\n\n"));
    }

    #[test]
    fn help_and_version_words_are_builtins() {
        let root = command();
        let p = Palette::off();
        let run = |parts: &[&str]| builtin_request(&root, &argv(parts), &p);

        let r = run(&["asana", "help"]).expect("help");
        assert_eq!((r.code, r.stdout), (0, render_help(&[], &p)));

        let r = run(&["asana", "--no-color", "help", "task", "ls"]).expect("help task ls");
        assert_eq!(r.code, 0);
        assert!(r.stdout.starts_with("List tasks"));
        assert!(r.stdout.contains("  asana task list [flags]\n"));

        // Like cobra, extra words after a matching command are ignored.
        let r = run(&["asana", "help", "task", "nope"]).expect("help task nope");
        assert_eq!(
            (r.code, r.stdout),
            (0, render_help(&["task".to_string()], &p))
        );

        let r = run(&["asana", "help", "exit-codes"]).expect("topic");
        assert_eq!(r.code, 0);
        assert!(r
            .stdout
            .starts_with("asana follows normal conventions regarding exit codes.\n"));
        assert!(r.stdout.ends_with("exits with 0.\n"));
        assert!(!r.stdout.contains("USAGE"));

        let r = run(&["asana", "help", "nope"]).expect("unknown topic");
        assert_eq!(r.code, 1);
        assert!(r.stdout.is_empty());
        assert!(r.stderr.starts_with(
            "Unknown help topic [`nope`]\nUsage:  asana <command> <subcommand> [flags]\n"
        ));

        let r = run(&["asana", "version"]).expect("version");
        assert_eq!(r.code, 0);
        assert_eq!(
            r.stdout,
            format!("asana version {VERSION}\nhttps://github.com/davediv/asana-cli/releases/tag/v{VERSION}\n")
        );

        // `help` anywhere but first is an ordinary word for clap to judge.
        assert!(run(&["asana", "task", "help"]).is_none());
        assert!(run(&["asana", "task", "list"]).is_none());
        assert!(run(&["asana", "--workspace", "help"]).is_none());
    }

    #[test]
    fn clap_errors_route_help_and_version_to_stdout() {
        let p = Palette::off();
        for parts in [
            vec!["asana", "help"],
            vec!["asana", "help", "task"],
            vec!["asana", "task", "--help"],
            vec!["asana", "-h"],
            vec!["asana", "version"],
            vec!["asana", "--version"],
        ] {
            let args = argv(&parts);
            let err = Cli::try_parse_from(&args).expect_err("clap reports these");
            let r = render_clap_error(&err, &args, &p);
            assert_eq!(r.code, 0, "{parts:?}");
            assert!(r.stderr.is_empty(), "{parts:?}");
            assert!(!r.stdout.is_empty(), "{parts:?}");
        }
        let args = argv(&["asana", "task", "help"]);
        let err = Cli::try_parse_from(&args).expect_err("unknown subcommand");
        let r = render_clap_error(&err, &args, &p);
        assert_eq!(r.code, 1);
        assert!(r
            .stderr
            .starts_with("unknown command \"help\" for \"asana task\"\n"));
    }

    #[test]
    fn suggests_close_commands() {
        let names = vec!["list".to_string(), "view".to_string(), "delete".to_string()];
        assert_eq!(suggestions("lsit", &names), vec!["list"]);
        assert_eq!(suggestions("vi", &names), vec!["view"]);
        assert!(suggestions("zzzzzz", &names).is_empty());
    }

    #[test]
    fn clap_tree_is_valid() {
        Cli::command().debug_assert();
    }
}
