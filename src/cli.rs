//! The clap command tree, global flags, and the gh-style help renderer.
//!
//! clap does the parsing; the help screens are rendered here from the built
//! `Command` tree so they read exactly like gh's. Error text (unknown
//! command, unknown flag, `accepts N arg(s)`, the `Usage:` block under usage
//! errors) lives in `error.rs`, which borrows the flag-table formatting from
//! here.
//!
//! Line length follows one rule on every screen. gh never rewraps: its
//! descriptions are hand-wrapped under 80 columns in the source and its
//! flag tables print as pflag lays them out. Ours are hand-wrapped the same
//! way (`cmd::help::PROSE_WIDTH`), and because a terminal narrower than a
//! line would break it mid-word, the renderer also breaks any line wider
//! than the terminal (capped at [`MAX_HELP_WIDTH`]) at a space, keeping the
//! column the text started in: prose keeps its indent, a bullet hangs, a
//! flag or command description continues under itself. Examples are
//! commands and print verbatim. Every piece's help gets this without
//! touching its text; only the width of the terminal decides.

use std::ffi::OsString;

use clap::{ArgAction, Command, CommandFactory, Parser, Subcommand};

use crate::cmd::{
    auth_status, help, project_list, project_view, task_assign, task_comment, task_complete,
    task_create, task_delete, task_list, task_view, workspace_list, Prevalidate as _,
};
use crate::render::Palette;

// The error types command modules raise live in `error.rs`; these names are
// kept here so `use crate::cli::{usage_error, Cancelled, ..}` keeps working.
#[allow(unused_imports)]
pub use crate::error::{usage_error, Cancelled, Rendered, SilentExit, UsageError};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The widest a help line is laid out for, whatever the terminal: prose
/// keeps a readable measure on a wide screen, and gh's own hand-wrapped
/// descriptions stay under this.
pub const MAX_HELP_WIDTH: usize = 100;

/// The width help is rendered at: the terminal's (stdout, else stderr, so
/// `asana --help | less` still sees the screen it is read on), else
/// `COLUMNS`, else 80, capped at [`MAX_HELP_WIDTH`].
fn help_width() -> usize {
    let detected = terminal_size::terminal_size()
        .map(|(terminal_size::Width(w), _)| w as usize)
        .filter(|w| *w > 0)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|c| c.trim().parse::<usize>().ok())
                .filter(|w| *w > 0)
        })
        .unwrap_or(80);
    detected.min(MAX_HELP_WIDTH)
}

/// Break one logical line into lines no wider than `limit`, the first
/// prefixed with `first` and the rest with `rest`. Greedy, at ASCII spaces
/// only, like a terminal would if it broke at words: no hyphenation, and a
/// word wider than the limit (a URL) stays whole on a line of its own. A
/// line that already fits comes back as it is.
fn wrap_line(text: &str, limit: usize, first: &str, rest: &str) -> Vec<String> {
    let text = text.trim_end();
    if text.is_empty() {
        return vec![first.trim_end().to_string()];
    }
    if crate::render::text::width(first) + crate::render::text::width(text) <= limit {
        return vec![format!("{first}{text}")];
    }
    let opts = textwrap::Options::new(limit)
        .initial_indent(first)
        .subsequent_indent(rest)
        .break_words(false)
        .word_separator(textwrap::WordSeparator::AsciiSpace)
        .word_splitter(textwrap::WordSplitter::NoHyphenation)
        .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit);
    textwrap::wrap(text, &opts)
        .into_iter()
        .map(|l| l.into_owned())
        .collect()
}

/// `<https://...>` written as a bare URL: the angle brackets are markdown's
/// autolink syntax and mean nothing on a terminal.
fn unbracket_urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(i) = rest.find("<http") {
        let after = &rest[i + 1..];
        match after.find('>') {
            Some(j)
                if (after.starts_with("http://") || after.starts_with("https://"))
                    && !after[..j].contains(char::is_whitespace) =>
            {
                out.push_str(&rest[..i]);
                out.push_str(&after[..j]);
                rest = &after[j + 1..];
            }
            _ => {
                out.push_str(&rest[..i + 1]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A block of prose (a description, ARGUMENTS, LEARN MORE) laid out under
/// `indent` and wrapped to `limit`: each line is broken at spaces if it must
/// be, a `- ` bullet's continuation hangs under its text, blank lines stay
/// empty (never indent-only), and URLs are bare.
fn prose(out: &mut String, text: &str, indent: &str, limit: usize) {
    for line in text.lines() {
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        let line = unbracket_urls(line);
        let body = line.trim_start();
        let lead = &line[..line.len() - body.len()];
        let hang = if body.starts_with("- ") || body.starts_with("* ") {
            "  "
        } else {
            ""
        };
        let first = format!("{indent}{lead}");
        let rest = format!("{indent}{lead}{hang}");
        for l in wrap_line(body, limit, &first, &rest) {
            out.push_str(&l);
            out.push('\n');
        }
    }
}

/// A two-column row (`  name:        description`, `  -s, --long   help`):
/// the description wraps under itself when the row is wider than `limit`,
/// as pflag's `FlagUsagesWrapped` does, unless the column leaves it fewer
/// than 24 cells, when it prints whole.
fn columns(out: &mut String, left: &str, desc: &str, limit: Option<usize>) {
    let col = crate::render::text::width(left);
    let room = limit.map(|l| l.saturating_sub(col));
    if desc.is_empty() || room.is_some_and(|r| r < 24) {
        out.push_str(left);
        out.push_str(desc);
        out.push('\n');
        return;
    }
    let rest = " ".repeat(col);
    for l in wrap_line(desc, limit.unwrap_or(usize::MAX), left, &rest) {
        out.push_str(&l);
        out.push('\n');
    }
}

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
    #[arg(long, global = true, value_name = "NAME")]
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
        about = help::AUTH_ABOUT,
        long_about = help::AUTH_LONG_ABOUT,
        after_help = help::AUTH_EXAMPLES
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

/// The one-line `about` of each leaf is what its group screen lists. The
/// list reads in one grammatical number, gh's singular (`Delete a task`,
/// `View a task`), so the two commands that take several tasks are named
/// here in the same form; their own `--help` still opens with "one or more".
#[derive(Subcommand, Debug)]
pub enum TaskCmd {
    #[command(visible_alias = "ls")]
    List(task_list::Args),
    View(task_view::Args),
    #[command(visible_alias = "new")]
    Create(task_create::Args),
    #[command(visible_alias = "close", about = "Mark a task complete")]
    Complete(task_complete::CompleteArgs),
    #[command(about = "Reopen a task")]
    Reopen(task_complete::ReopenArgs),
    Assign(task_assign::Args),
    Comment(task_comment::Args),
    Delete(task_delete::Args),
}

impl Commands {
    /// Checks that need neither a token nor the network, run before
    /// `config::load`: the shared identifier checks here (bad identifiers
    /// fail fast like gh's `invalid issue format: "abc"`), then the
    /// command's own `Prevalidate::prevalidate`. `interactive` is gh's
    /// `CanPrompt` (stdin, stdout and stderr are all terminals).
    pub fn prevalidate(&self, interactive: bool) -> anyhow::Result<()> {
        use crate::resolve::parse_task_id;
        let check_all =
            |tasks: &[String]| tasks.iter().try_for_each(|t| parse_task_id(t).map(drop));
        match self {
            Commands::Task(TaskCmd::View(a)) => parse_task_id(&a.task).map(drop)?,
            Commands::Task(TaskCmd::Assign(a)) => parse_task_id(&a.task).map(drop)?,
            Commands::Task(TaskCmd::Comment(a)) => parse_task_id(&a.task).map(drop)?,
            Commands::Task(TaskCmd::Delete(a)) => parse_task_id(&a.task).map(drop)?,
            Commands::Task(TaskCmd::Complete(a)) => check_all(&a.tasks)?,
            Commands::Task(TaskCmd::Reopen(a)) => check_all(&a.tasks)?,
            Commands::Task(TaskCmd::Create(a)) => {
                a.due
                    .as_deref()
                    .map(|d| task_create::parse_due(d, crate::render::time::today()))
                    .transpose()?;
            }
            Commands::Task(TaskCmd::List(a)) if a.limit == 0 => {
                anyhow::bail!("invalid limit: {}", a.limit)
            }
            Commands::Project(ProjectCmd::List(a)) if a.limit == 0 => {
                anyhow::bail!("invalid limit: {}", a.limit)
            }
            _ => {}
        }
        match self {
            Commands::Auth(AuthCmd::Status(a)) => a.prevalidate(interactive),
            Commands::Workspace(WorkspaceCmd::List(a)) => a.prevalidate(interactive),
            Commands::Project(ProjectCmd::List(a)) => a.prevalidate(interactive),
            Commands::Project(ProjectCmd::View(a)) => a.prevalidate(interactive),
            Commands::Task(TaskCmd::List(a)) => a.prevalidate(interactive),
            Commands::Task(TaskCmd::View(a)) => a.prevalidate(interactive),
            Commands::Task(TaskCmd::Create(a)) => a.prevalidate(interactive),
            Commands::Task(TaskCmd::Complete(a)) => a.prevalidate(interactive),
            Commands::Task(TaskCmd::Reopen(a)) => a.prevalidate(interactive),
            Commands::Task(TaskCmd::Assign(a)) => a.prevalidate(interactive),
            Commands::Task(TaskCmd::Comment(a)) => a.prevalidate(interactive),
            Commands::Task(TaskCmd::Delete(a)) => a.prevalidate(interactive),
        }
    }

    /// Whether the invoked leaf asked for `--json`.
    pub fn wants_json(&self) -> bool {
        match self {
            Commands::Workspace(WorkspaceCmd::List(a)) => a.json,
            Commands::Project(ProjectCmd::List(a)) => a.json,
            Commands::Project(ProjectCmd::View(a)) => a.json,
            Commands::Task(TaskCmd::List(a)) => a.json,
            Commands::Task(TaskCmd::View(a)) => a.json,
            _ => false,
        }
    }
}

/// The fully built command tree (globals propagated into subcommands).
pub fn command() -> Command {
    let mut cmd = Cli::command();
    cmd.build();
    cmd
}

/// The command at `path` (names, not aliases) in the built tree.
pub(crate) fn find<'a>(root: &'a Command, path: &[String]) -> Option<&'a Command> {
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

/// `asana help [<command>... | <topic>]`, `asana <topic>` and `asana
/// version`, which gh supports and clap reports as unknown subcommands.
/// `help` follows the command tree as far as it matches, like cobra's; a
/// first word that is neither a command nor a topic is `Unknown help
/// topic`, cobra's words, with the usage block after a blank line as under
/// every other usage error. gh registers its topics as hidden commands, so
/// `gh formatting` and `gh formatting --help` print the topic too; a topic
/// given with any flag but `--help`/`--no-color` is left to clap's
/// unknown-command report.
pub(crate) fn builtin_request(
    root: &Command,
    argv: &[OsString],
    palette: &Palette,
) -> Option<Rendered> {
    let ok = |stdout: String| Rendered {
        stdout,
        stderr: String::new(),
        code: 0,
    };
    let words = bare_words(argv);
    let only_help_flags = || {
        argv.iter().skip(1).all(|a| {
            let s = a.to_string_lossy();
            !s.starts_with('-') || matches!(s.as_ref(), "--help" | "-h" | "--no-color")
        })
    };
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
                    stderr: format!(
                        "Unknown help topic [`{first}`]\n\n{}",
                        crate::error::usage_block(&[])
                    ),
                    code: 1,
                });
            }
            Some(ok(render_help(&path, palette)))
        }
        Some(topic) if words.len() == 1 && only_help_flags() => {
            help::topic(topic).map(|t| ok(render_topic(t)))
        }
        _ => None,
    }
}

fn strs(path: &[String]) -> Vec<&str> {
    path.iter().map(String::as_str).collect()
}

/// The usage line for a command path: from `help::usage`, else
/// `asana <path> [<command>] [flags]`.
pub(crate) fn usage_line(path: &[String]) -> String {
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
pub(crate) struct FlagLine {
    left: String,
    help: String,
}

/// gh lists `--help` without its `-h` shorthand.
fn short_of(a: &clap::Arg) -> Option<char> {
    a.get_short().filter(|_| a.get_long() != Some("help"))
}

/// One flag table, laid out as pflag does and then dedented as gh does: a
/// long-only flag sits in a four-space gutter (`    --json`) so its `--`
/// lines up under `-s, --long`, and the gutter collapses when no flag in
/// the table has a short form (`gh issue delete --help` prints `  --yes`).
/// Usage errors print one table, so the gutter is decided here; help
/// screens print FLAGS and INHERITED FLAGS together and decide it once for
/// both with `flag_lines_with_gutter`.
pub(crate) fn flag_lines(args: Vec<&clap::Arg>) -> Vec<FlagLine> {
    let gutter = args.iter().any(|a| short_of(a).is_some());
    flag_lines_with_gutter(args, gutter)
}

/// `flag_lines` with the gutter decided by the caller. The value placeholder
/// is the flag's `value_name` verbatim (gh's `int`, `string`, `file`, and
/// `[HOST/]OWNER/REPO` for its inherited flag), else `string`.
pub(crate) fn flag_lines_with_gutter(args: Vec<&clap::Arg>, gutter: bool) -> Vec<FlagLine> {
    let mut args = args;
    args.sort_by_key(|a| a.get_long().unwrap_or("").to_string());
    args.iter()
        .map(|a| {
            let mut left = String::new();
            match short_of(a) {
                Some(s) => left.push_str(&format!("-{s}, ")),
                None if gutter => left.push_str("    "),
                None => {}
            }
            left.push_str(&format!("--{}", a.get_long().unwrap_or("")));
            let takes_value = a.get_num_args().is_some_and(|n| n.takes_values());
            if takes_value {
                let name = a
                    .get_value_names()
                    .and_then(|v| v.first())
                    .map(|v| v.to_string())
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

/// The flag table as it sits under a usage error: two-space indent, the
/// descriptions aligned three spaces past the longest flag, never wrapped
/// (cobra prints `FlagUsages()` as is there).
pub(crate) fn render_flag_table(lines: &[FlagLine]) -> String {
    flag_table(lines, "  ", None)
}

/// The same table under a help heading, wrapped to `limit`.
fn flag_table(lines: &[FlagLine], indent: &str, limit: Option<usize>) -> String {
    let max = lines
        .iter()
        .map(|l| l.left.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for l in lines {
        let pad = " ".repeat(max - l.left.chars().count());
        let left = format!("{indent}{}{}   ", l.left, pad);
        columns(&mut out, &left, &l.help, limit);
    }
    out
}

pub(crate) fn local_flags(cmd: &Command) -> Vec<&clap::Arg> {
    cmd.get_arguments()
        .filter(|a| !a.is_positional() && !a.is_hide_set() && !a.is_global_set())
        .collect()
}

fn inherited_flags(cmd: &Command) -> Vec<&clap::Arg> {
    cmd.get_arguments()
        .filter(|a| !a.is_positional() && !a.is_hide_set() && a.is_global_set())
        .collect()
}

pub(crate) fn root_flags(cmd: &Command) -> Vec<&clap::Arg> {
    cmd.get_arguments()
        .filter(|a| !a.is_positional() && !a.is_hide_set())
        .collect()
}

/// `asana version` / `asana --version`, two lines like gh's.
pub fn render_version() -> String {
    format!(
        "asana version {VERSION}\n{}/v{VERSION}\n",
        help::RELEASES_URL
    )
}

/// `asana help <topic>`: the topic text alone, like gh's help topics, and
/// when the topic has examples a plain (never coloured) `EXAMPLES` heading
/// after two blank lines with the examples indented, as `gh help
/// formatting` prints them.
pub fn render_topic(topic: &help::Topic) -> String {
    render_topic_at(topic, help_width())
}

/// `render_topic` laid out for `width` columns: the prose wraps like a help
/// screen's, the reference (markdown) and the examples print verbatim.
pub fn render_topic_at(topic: &help::Topic, width: usize) -> String {
    let mut out = String::new();
    match topic.body {
        help::Body::Text(text) => prose(&mut out, text, "", width),
        help::Body::Reference => out.push_str(&render_reference(&command())),
    };
    if !topic.example.is_empty() {
        out.push_str("\n\nEXAMPLES\n");
        examples(&mut out, topic.example);
    }
    out
}

/// An EXAMPLES body under its two-space indent: commands print verbatim,
/// blank lines between groups stay empty.
fn examples(out: &mut String, text: &str) {
    for line in text.trim_end().lines() {
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

/// `asana help reference`: every command with its usage line, description
/// and flags, in the markdown `gh help reference` prints: a `#` heading per
/// level, the flags in a code fence, commands in name order.
pub fn render_reference(root: &Command) -> String {
    fn walk(out: &mut String, cmd: &Command, path: &mut Vec<String>, depth: usize) {
        out.push_str(&format!("{} `{}`\n\n", "#".repeat(depth), usage_line(path)));
        let about = cmd.get_about().map(|s| s.to_string()).unwrap_or_default();
        out.push_str(&format!("{about}\n\n"));
        let lines = flag_lines(local_flags(cmd));
        if !lines.is_empty() {
            out.push_str("```\n");
            out.push_str(&flag_table(&lines, "", None));
            out.push_str("```\n\n");
        }
        for sub in sorted_subcommands(cmd) {
            path.push(sub.get_name().to_string());
            walk(out, sub, path, depth + 1);
            path.pop();
        }
    }
    let mut out = String::from("# asana reference\n\n");
    let mut path = Vec::new();
    for sub in sorted_subcommands(root) {
        path.push(sub.get_name().to_string());
        walk(&mut out, sub, &mut path, 2);
        path.pop();
    }
    out
}

/// The visible subcommands in name order, as cobra lists them.
fn sorted_subcommands(cmd: &Command) -> Vec<&Command> {
    let mut subs: Vec<&Command> = cmd.get_subcommands().filter(|c| !c.is_hide_set()).collect();
    subs.sort_by_key(|c| c.get_name().to_string());
    subs
}

/// Render the gh-shaped help screen for a command path, laid out for the
/// terminal (see the module notes on line length).
pub fn render_help(path: &[String], palette: &Palette) -> String {
    render_help_at(path, palette, help_width())
}

/// `render_help` laid out for `width` columns.
pub fn render_help_at(path: &[String], palette: &Palette, width: usize) -> String {
    let root = command();
    if find(&root, path).is_none() {
        return render_help_for(&root, &[], palette, width);
    }
    render_help_for(&root, path, palette, width)
}

/// `render_help_at` over a given tree; `path` must exist in it.
fn render_help_for(root: &Command, path: &[String], palette: &Palette, width: usize) -> String {
    let cmd = find(root, path).expect("path exists in the tree");
    let heading = |s: &str| palette.bold(s);
    let mut out = String::new();

    // gh prints the description as written and then a blank line, so a
    // `long_about` that ends with a newline (as gh's heredoc ones do) leaves
    // two blank lines before USAGE, and a one-liner leaves one.
    let about = if path.is_empty() {
        help::ROOT_ABOUT.to_string()
    } else {
        cmd.get_long_about()
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| cmd.get_about().map(|s| s.to_string()))
            .unwrap_or_default()
    };
    prose(&mut out, &about, "", width);
    if about.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');

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
        // cobra pads `name:` to the longest name (never under 11) plus four,
        // so the column is 15 wide until a name passes eleven characters.
        // gh's help topics are commands too, so they count at the root.
        let topic_names = help::TOPICS
            .iter()
            .filter(|_| path.is_empty())
            .map(|t| t.name);
        let pad = subs
            .iter()
            .map(|c| c.get_name())
            .chain(topic_names)
            .map(str::len)
            .max()
            .unwrap_or(0)
            .max(11)
            + 4;
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
        // gh: `AVAILABLE COMMANDS` when a command has no groups, otherwise
        // the ungrouped ones go under `ADDITIONAL COMMANDS`.
        if !rest.is_empty() {
            let title = if sections.is_empty() {
                "AVAILABLE COMMANDS"
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
                let left = format!("  {:<pad$}", format!("{}:", c.get_name()), pad = pad);
                columns(&mut out, &left, &about, Some(width));
            }
            out.push('\n');
        }
        if path.is_empty() {
            out.push_str(&heading("HELP TOPICS"));
            out.push('\n');
            for t in help::TOPICS {
                let left = format!("  {:<pad$}", format!("{}:", t.name), pad = pad);
                columns(&mut out, &left, t.short, Some(width));
            }
            out.push('\n');
        }
    }

    let (local, inherited) = if path.is_empty() {
        (root_flags(cmd), Vec::new())
    } else {
        (local_flags(cmd), inherited_flags(cmd))
    };
    // One gutter for both tables: on every gh leaf screen the `--` of FLAGS
    // and INHERITED FLAGS line up (its inherited `-R, --repo` sees to that),
    // while its root and group screens, with no short flag anywhere, print
    // `  --help` flush left. Our inherited flags have no short forms, so the
    // decision is taken over the whole screen rather than per table.
    let gutter = local
        .iter()
        .chain(inherited.iter())
        .any(|a| short_of(a).is_some());
    let local_lines = flag_lines_with_gutter(local, gutter);
    if !local_lines.is_empty() {
        out.push_str(&heading("FLAGS"));
        out.push('\n');
        out.push_str(&flag_table(&local_lines, "  ", Some(width)));
        out.push('\n');
    }
    let inherited_lines = flag_lines_with_gutter(inherited, gutter);
    if !inherited_lines.is_empty() {
        out.push_str(&heading("INHERITED FLAGS"));
        out.push('\n');
        out.push_str(&flag_table(&inherited_lines, "  ", Some(width)));
        out.push('\n');
    }

    if let Some(text) = help::arguments(&strs(path)) {
        out.push_str(&heading("ARGUMENTS"));
        out.push('\n');
        prose(&mut out, text, "  ", width);
        out.push('\n');
    }

    let example_text = if path.is_empty() {
        Some(help::ROOT_EXAMPLES.to_string())
    } else {
        cmd.get_after_help().map(|s| s.to_string())
    };
    if let Some(ex) = example_text.filter(|e| !e.trim().is_empty()) {
        out.push_str(&heading("EXAMPLES"));
        out.push('\n');
        examples(&mut out, &ex);
        out.push('\n');
    }

    out.push_str(&heading("LEARN MORE"));
    out.push('\n');
    prose(&mut out, &help::learn_more().join("\n"), "  ", width);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    /// The screens as the critic captures them: a 120-column pty, so the
    /// layout width is the cap.
    fn help(path: &[&str], palette: &Palette) -> String {
        let path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
        render_help_at(&path, palette, MAX_HELP_WIDTH)
    }

    /// Every command path in the tree, the root first.
    fn all_paths() -> Vec<Vec<String>> {
        fn walk(cmd: &Command, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
            out.push(path.clone());
            for sub in cmd.get_subcommands().filter(|c| !c.is_hide_set()) {
                path.push(sub.get_name().to_string());
                walk(sub, path, out);
                path.pop();
            }
        }
        let root = command();
        let mut out = Vec::new();
        walk(&root, &mut Vec::new(), &mut out);
        out
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
        let root = help(&[], &p);
        assert!(root.starts_with(help::ROOT_ABOUT));
        for section in ["USAGE", "CORE COMMANDS", "FLAGS", "EXAMPLES", "LEARN MORE"] {
            assert!(
                root.contains(&format!("\n{section}\n")),
                "{section} missing in:\n{root}"
            );
        }
        let task = help(&["task"], &p);
        assert!(task.contains("\nGENERAL COMMANDS\n"));
        assert!(task.contains("\nTARGETED COMMANDS\n"));
        assert!(task.contains("\nARGUMENTS\n"));
        assert!(task.contains("  asana task <command> [flags]"));
        let list = help(&["task", "list"], &p);
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
  auth:          Authenticate with Asana
  project:       Manage projects
  task:          Manage tasks
  workspace:     Manage workspaces

HELP TOPICS
  environment:   Environment variables that can be used with asana
  exit-codes:    Exit codes used by asana
  formatting:    Formatting options for JSON and piped output from asana
  reference:     A comprehensive reference of all asana commands

FLAGS
  --help             Show help for command
  --no-color         Disable color output
  --version          Show asana version
  --workspace NAME   Select the workspace by name or gid

EXAMPLES
  # Create a personal access token at https://app.asana.com/0/my-apps
  $ export ASANA_TOKEN=<token>
  $ asana auth status

  # Work with tasks
  $ asana task list
  $ asana task create --title \"Fix the login page\"
  $ asana task view 1211234567890123 --web

LEARN MORE
  Use `asana <command> <subcommand> --help` for more information on a command.
  Read the manual at https://github.com/davediv/asana-cli#readme
  Learn about exit codes using `asana help exit-codes`
  Learn about environment variables using `asana help environment`
  Learn about output formatting using `asana help formatting`

";
        assert_eq!(help(&[], &Palette::off()), expected);
        // Every line fits an 80-column terminal, so the screen reads the
        // same at 80 as at the critic's 120.
        assert_eq!(render_help_at(&[], &Palette::off(), 80), expected);
        assert!(!expected.contains("\n  \n"), "whitespace-only line");
    }

    #[test]
    fn headings_use_gh_colour_bytes_only_when_enabled() {
        let coloured = help(&[], &Palette::new(true));
        assert!(coloured.contains("\x1b[0;1;39mUSAGE\x1b[0m\n"));
        assert!(coloured.contains("\x1b[0;1;39mLEARN MORE\x1b[0m\n"));
        // Nothing but the headings is styled.
        let stripped = coloured.replace("\x1b[0;1;39m", "").replace("\x1b[0m", "");
        assert_eq!(stripped, help(&[], &Palette::off()));
        assert!(!help(&[], &Palette::off()).contains('\x1b'));
    }

    #[test]
    fn group_help_lists_inherited_flags_and_arguments() {
        let p = Palette::off();
        let project = help(&["project"], &p);
        let expected_tail = "\
INHERITED FLAGS
  --help             Show help for command
  --no-color         Disable color output
  --workspace NAME   Select the workspace by name or gid

ARGUMENTS
  A project can be supplied as argument in any of the following formats:
";
        assert!(project.contains(expected_tail), "{project}");
        assert!(!project.contains("\nFLAGS\n"));
        assert!(!project.contains("HELP TOPICS"));
        assert!(project.ends_with("`asana help formatting`\n\n"));
    }

    /// Every description on the task screen names one task, the way gh's
    /// `Delete issue` and `View an issue` do; `complete` and `reopen` take
    /// several but are listed in the same number as their siblings.
    #[test]
    fn task_group_descriptions_agree_in_number() {
        let p = Palette::off();
        let task = help(&["task"], &p);
        let expected = "\
TARGETED COMMANDS
  assign:        Assign a task to someone
  comment:       Add a comment to a task
  complete:      Mark a task complete
  delete:        Delete a task
  reopen:        Reopen a task
  view:          View a task

";
        assert!(task.contains(expected), "{task}");
        assert!(!task.contains("tasks complete"), "{task}");
        assert!(!task.contains("Reopen tasks"), "{task}");
    }

    /// `asana help formatting` reads like `gh help formatting`: the prose,
    /// two blank lines, a plain `EXAMPLES` heading and the examples indented
    /// two spaces (blank lines included), with no colour anywhere.
    #[test]
    fn formatting_topic_has_examples_like_gh() {
        let topic = help::topic("formatting").expect("formatting topic");
        let out = render_topic_at(topic, MAX_HELP_WIDTH);
        assert!(out.starts_with("By default, the result of `asana` commands is output in line-based plain\ntext format"), "{out}");
        assert!(out.contains("`--json`"), "{out}");
        assert!(out.contains("tab-separated with no header"), "{out}");
        assert!(
            out.contains(".\n\n\nEXAMPLES\n  # Default output format\n  $ asana task list\n\n"),
            "{out}"
        );
        assert!(
            out.contains("\n  $ asana task list --json\n  [\n    {\n"),
            "{out}"
        );
        assert!(
            out.ends_with("\n  https://app.asana.com/0/1200000000000000/1211234567890123\n"),
            "{out}"
        );
        assert!(!out.contains('\x1b'));
        assert!(!out.contains("USAGE"));
        assert!(!out.contains("\n  \n"), "whitespace-only line in {out}");
        // Topics without examples end with their prose.
        let exit = render_topic_at(help::topic("exit-codes").expect("exit-codes"), 80);
        assert!(!exit.contains("EXAMPLES"));
        assert!(exit.ends_with("exits with 0.\n"));
        // The bracketed-URL and hand-wrap rules hold on every topic at 80.
        for t in help::TOPICS {
            let out = render_topic_at(t, 80);
            for line in out.lines() {
                assert!(!line.contains("<http"), "{}: {line}", t.name);
                if matches!(t.body, help::Body::Text(_)) {
                    assert!(line.chars().count() <= 80, "{}: {line}", t.name);
                }
            }
        }
    }

    /// A group without groupings of its own lists `AVAILABLE COMMANDS`, as
    /// `gh auth --help` does; `asana auth --help` is judged against it.
    #[test]
    fn ungrouped_commands_are_available_commands() {
        let p = Palette::off();
        let auth = help(&["auth"], &p);
        assert!(
            auth.contains(
                "\nUSAGE\n  asana auth <command> [flags]\n\nAVAILABLE COMMANDS\n  status:        "
            ),
            "{auth}"
        );
        assert!(!auth.contains("\nCOMMANDS\n"));
        assert!(!auth.contains("ADDITIONAL COMMANDS"));
        let ws = help(&["workspace"], &p);
        assert!(
            ws.contains("\nAVAILABLE COMMANDS\n  list:          "),
            "{ws}"
        );
    }

    /// `asana auth --help` is the screen a new user opens to log in, so it
    /// says how: the token page, the environment variable, the config file,
    /// and the topic with the details. Its first line is the root screen's
    /// `auth:` summary, as gh's is, and the root EXAMPLES show the same
    /// first-run path before anything else. The paragraph is hand-wrapped
    /// short of 78 columns with a bare URL, so it reads the same at 80 and
    /// at 120.
    #[test]
    fn auth_screens_say_how_to_authenticate() {
        let p = Palette::off();
        let auth = help(&["auth"], &p);
        let expected_head = "\
Authenticate with Asana.

asana authenticates with a personal access token. Create one at
https://app.asana.com/0/my-apps, then either set the ASANA_TOKEN environment
variable to it or put `token = \"...\"` in ~/.config/asana/config.toml.
Run `asana auth status` to check which account the token belongs to;
`asana help environment` has the details.

USAGE
  asana auth <command> [flags]

AVAILABLE COMMANDS
  status:        ";
        assert!(auth.starts_with(expected_head), "{auth}");
        assert_eq!(render_help_at(&["auth".to_string()], &p, 80), auth);
        assert!(
            auth.contains("\nEXAMPLES\n  $ export ASANA_TOKEN=<token>\n  $ asana auth status\n\n"),
            "{auth}"
        );
        assert!(!auth.contains('<') || !auth.contains("<http"), "{auth}");
        for line in auth.lines() {
            assert!(line.chars().count() <= 80, "auth line too long: {line}");
        }
        let root = help(&[], &p);
        assert!(
            root.contains(&format!("\n  auth:          {}\n", help::AUTH_ABOUT)),
            "{root}"
        );
        assert!(help::AUTH_LONG_ABOUT.starts_with(&format!("{}.\n", help::AUTH_ABOUT)));
        let examples = root
            .split("\nEXAMPLES\n")
            .nth(1)
            .and_then(|s| s.split("\nLEARN MORE\n").next())
            .expect("root EXAMPLES");
        assert!(
            examples.starts_with(
                "  # Create a personal access token at https://app.asana.com/0/my-apps\n  $ export ASANA_TOKEN=<token>\n  $ asana auth status\n\n  # Work with tasks\n"
            ),
            "{examples}"
        );
        for line in examples.lines() {
            assert!(
                line.is_empty() || line.starts_with("  $ ") || line.starts_with("  # "),
                "not an example line: {line:?}"
            );
        }
    }

    /// One rule for every screen: at 80 columns no line of any help screen
    /// is wider than the terminal, however long a piece wrote its text, and
    /// at the critic's 120 columns a line under the cap is left exactly as
    /// written. Examples are the exception: they are commands and print
    /// verbatim.
    #[test]
    fn every_screen_fits_the_terminal() {
        let p = Palette::off();
        for path in all_paths() {
            let at80 = render_help_at(&path, &p, 80);
            let mut in_examples = false;
            for line in at80.lines() {
                if line == "EXAMPLES" {
                    in_examples = true;
                } else if line == "LEARN MORE" {
                    in_examples = false;
                }
                assert!(!line.contains("<http"), "{path:?}: bracketed URL: {line}");
                assert!(
                    line.is_empty() || !line.trim().is_empty(),
                    "{path:?}: whitespace-only line"
                );
                if in_examples {
                    continue;
                }
                assert!(line.chars().count() <= 80, "{path:?} at 80 columns: {line}");
            }
            // Wrapping only adds lines: the wide layout is the narrow one
            // with its continuation lines joined back.
            let wide = render_help_at(&path, &p, MAX_HELP_WIDTH);
            assert!(wide.lines().count() <= at80.lines().count(), "{path:?}");
            for line in wide.lines() {
                assert!(
                    line.chars().count() <= MAX_HELP_WIDTH
                        || line.starts_with("  $ ")
                        || line.split_whitespace().any(|w| w.chars().count() > 60),
                    "{path:?} at {MAX_HELP_WIDTH} columns: {line}"
                );
            }
        }
    }

    /// The wrapper is greedy, breaks only at spaces, hangs bullets and
    /// flag descriptions under their column, and leaves a lone long word (a
    /// URL) whole.
    #[test]
    fn wrapping_keeps_columns_and_words() {
        assert_eq!(wrap_line("short", 20, "  ", "  "), vec!["  short"]);
        assert_eq!(
            wrap_line("one two three four five", 14, "  ", "    "),
            vec!["  one two", "    three four", "    five"]
        );
        assert_eq!(
            wrap_line(
                "see https://example.com/a/very/long/path/indeed now",
                20,
                "",
                ""
            ),
            vec!["see", "https://example.com/a/very/long/path/indeed", "now"]
        );
        // No hyphenation: `case-insensitive` never splits at its hyphen.
        assert_eq!(
            wrap_line("a case-insensitive name", 17, "", ""),
            vec!["a", "case-insensitive", "name"]
        );

        let mut out = String::new();
        prose(
            &mut out,
            "- by gid, e.g. \"1211234567890123\"; or\n\nplain <https://x.y/z> end",
            "  ",
            24,
        );
        assert_eq!(
            out,
            "  - by gid, e.g.\n    \"1211234567890123\";\n    or\n\n  plain https://x.y/z\n  end\n"
        );

        let mut out = String::new();
        columns(
            &mut out,
            "  -F, --file file   ",
            "Read the body from file (use \"-\" for stdin)",
            Some(50),
        );
        assert_eq!(
            out,
            "  -F, --file file   Read the body from file (use\n                    \"-\" for stdin)\n"
        );
        // Too little room for the description: printed whole.
        let mut out = String::new();
        columns(
            &mut out,
            "  -F, --file file   ",
            "Read the body from file",
            Some(30),
        );
        assert_eq!(out, "  -F, --file file   Read the body from file\n");
        let mut out = String::new();
        columns(&mut out, "  --workspace NAME   ", "", Some(80));
        assert_eq!(out, "  --workspace NAME   \n");

        assert_eq!(
            unbracket_urls("at <https://a.b/c>, then"),
            "at https://a.b/c, then"
        );
        assert_eq!(unbracket_urls("<https://a.b/c>"), "https://a.b/c");
        assert_eq!(
            unbracket_urls("key:<TAB>value <not a url>"),
            "key:<TAB>value <not a url>"
        );
        assert_eq!(unbracket_urls("<https://a.b/c d>"), "<https://a.b/c d>");
        assert_eq!(unbracket_urls("<http"), "<http");
    }

    /// The flag gutter follows gh: on a screen where some flag has a short
    /// form, every long-only flag in FLAGS and INHERITED FLAGS sits four
    /// spaces in so the `--` columns line up; on a screen with none (the
    /// root, the groups) the tables print flush against the indent.
    #[test]
    fn flag_gutter_is_decided_per_screen() {
        let p = Palette::off();
        let list = help(&["task", "list"], &p);
        assert!(list.contains("\n  -L, --limit int"), "{list}");
        assert!(
            list.contains(
                "\nINHERITED FLAGS\n      --help             Show help for command\n      --no-color         Disable color output\n      --workspace NAME   Select the workspace by name or gid\n"
            ),
            "{list}"
        );
        let task = help(&["task"], &p);
        assert!(
            task.contains("\nINHERITED FLAGS\n  --help             Show help for command\n"),
            "{task}"
        );
        let root = help(&[], &p);
        assert!(
            root.contains("\nFLAGS\n  --help             Show help for command\n"),
            "{root}"
        );
    }

    /// Usage errors print one table, so its gutter depends on that table
    /// alone, like gh's dedent of `LocalFlags().FlagUsages()`.
    #[test]
    fn flag_table_alone_collapses_its_gutter() {
        let root = command();
        let lines = flag_lines(root_flags(&root));
        let table = render_flag_table(&lines);
        assert!(
            table.starts_with("  --help             Show help for command\n"),
            "{table}"
        );
        let list = find(&root, &["task".to_string(), "list".to_string()]).unwrap();
        let table = render_flag_table(&flag_lines(local_flags(list)));
        assert!(table.contains("\n      --json"), "{table}");
        assert!(table.contains("\n  -L, --limit int"), "{table}");
    }

    /// gh prints the description exactly as written and then a blank line:
    /// a one-line `about` leaves one blank line before USAGE, and a
    /// heredoc-style `long_about` ending in a newline leaves two, the way
    /// `gh issue create --help` does. Value names print verbatim.
    #[test]
    fn description_is_printed_verbatim() {
        let p = Palette::off();
        let mut root = Command::new("asana")
            .subcommand(
                Command::new("one")
                    .about("One line")
                    .long_about("First paragraph.\n\nSecond paragraph.\n"),
            )
            .subcommand(
                Command::new("two").about("Two").arg(
                    clap::Arg::new("workspace")
                        .long("workspace")
                        .value_name("NAME"),
                ),
            );
        root.build();
        let one = render_help_for(&root, &["one".to_string()], &p, 80);
        assert!(
            one.starts_with(
                "First paragraph.\n\nSecond paragraph.\n\n\nUSAGE\n  asana one [flags]\n"
            ),
            "{one}"
        );
        let two = render_help_for(&root, &["two".to_string()], &p, 80);
        assert!(two.starts_with("Two\n\nUSAGE\n"), "{two}");
        assert!(two.contains("\n  --workspace NAME   \n"), "{two}");
    }

    /// Another piece's text that overruns the terminal is broken at a space
    /// with the column kept, and left alone on a terminal wide enough.
    #[test]
    fn overlong_text_from_a_piece_wraps_at_narrow_widths_only() {
        let p = Palette::off();
        let mut root = Command::new("asana").subcommand(
            Command::new("wide")
                .disable_help_flag(true)
                .about("Do a thing")
                .long_about(
                    "A project can be given by gid, by URL, or by name (case-insensitive; a unique prefix is enough).\n\n\
                     The `--assignee` flag supports the following special values:\n\
                     - `@me`: assign yourself, or anyone else by email, by user gid, or by name when it is unique",
                )
                .arg(
                    clap::Arg::new("notes-file")
                        .short('F')
                        .long("notes-file")
                        .value_name("file")
                        .help("Read the description from file (use \"-\" to read from standard input)"),
                )
                .after_help("$ asana wide --notes-file - < a-very-long-file-name-that-nobody-would-ever-break-across-lines.txt"),
        );
        root.build();
        let path = ["wide".to_string()];
        let narrow = render_help_for(&root, &path, &p, 80);
        let expected_head = "\
A project can be given by gid, by URL, or by name (case-insensitive; a unique
prefix is enough).

The `--assignee` flag supports the following special values:
- `@me`: assign yourself, or anyone else by email, by user gid, or by name when
  it is unique

USAGE
  asana wide [flags]

FLAGS
  -F, --notes-file file   Read the description from file (use \"-\" to read from
                          standard input)

EXAMPLES
  $ asana wide --notes-file - < a-very-long-file-name-that-nobody-would-ever-break-across-lines.txt
";
        assert!(narrow.starts_with(expected_head), "{narrow}");
        let wide = render_help_for(&root, &path, &p, MAX_HELP_WIDTH);
        assert!(
            wide.starts_with("A project can be given by gid, by URL, or by name (case-insensitive; a unique prefix is enough).\n\n"),
            "{wide}"
        );
        assert!(
            wide.contains("\n  -F, --notes-file file   Read the description from file (use \"-\" to read from standard input)\n"),
            "{wide}"
        );
    }

    #[test]
    fn usage_lines_name_optional_arguments() {
        assert_eq!(
            usage_line(&["task".to_string(), "assign".to_string()]),
            "asana task assign {<gid> | <url>} [<assignee>] [flags]"
        );
        assert_eq!(
            usage_line(&["task".to_string()]),
            "asana task <command> [flags]"
        );
        assert_eq!(usage_line(&[]), "asana <command> <subcommand> [flags]");
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

        let r = run(&["asana", "help", "formatting"]).expect("formatting topic");
        assert_eq!(r.code, 0);
        assert_eq!(r.stdout, render_topic(help::topic("formatting").unwrap()));

        // Topics are hidden commands in gh: `gh formatting`, `gh formatting --help`.
        for (parts, name) in [
            (&["asana", "formatting"][..], "formatting"),
            (&["asana", "formatting", "--help"], "formatting"),
            (&["asana", "--no-color", "exit-codes", "-h"], "exit-codes"),
        ] {
            let r = run(parts).expect("topic as a command");
            let topic = help::topic(name).unwrap();
            assert_eq!((r.code, r.stdout), (0, render_topic(topic)), "{parts:?}");
        }
        // With another word or flag it is an ordinary unknown command.
        assert!(run(&["asana", "formatting", "list"]).is_none());
        assert!(run(&["asana", "formatting", "--json"]).is_none());
        assert!(run(&["asana", "--workspace", "x", "formatting"]).is_none());

        let r = run(&["asana", "help", "reference"]).expect("reference");
        assert_eq!(r.code, 0);
        assert!(r.stdout.starts_with("# asana reference\n\n## `asana auth <command> [flags]`\n\nAuthenticate with Asana\n\n### `asana auth status [flags]`\n\n"), "{}", r.stdout);
        assert!(r.stdout.contains("\n## `asana task <command> [flags]`\n\nManage tasks\n\n### `asana task assign {<gid> | <url>} [<assignee>] [flags]`\n"), "{}", r.stdout);
        assert!(
            r.stdout.contains(
                "\n### `asana task complete {<gids> | <urls>} [flags]`\n\nMark a task complete\n"
            ),
            "{}",
            r.stdout
        );
        assert!(r.stdout.contains("\n```\n    --json"), "{}", r.stdout);
        assert!(r.stdout.contains("\n-L, --limit int"), "{}", r.stdout);
        assert!(!r.stdout.contains("--help"), "{}", r.stdout);
        assert!(!r.stdout.contains("--workspace"), "{}", r.stdout);
        assert!(r.stdout.ends_with("```\n\n"), "{}", r.stdout);

        // Cobra's words, then the usage block after a blank line like every
        // other usage error (`gh-errors/unknown-flag.txt`).
        let r = run(&["asana", "help", "nope"]).expect("unknown topic");
        assert_eq!(r.code, 1);
        assert!(r.stdout.is_empty());
        assert!(
            r.stderr.starts_with(
                "Unknown help topic [`nope`]\n\nUsage:  asana <command> <subcommand> [flags]\n\nAvailable commands:\n  auth\n"
            ),
            "{}",
            r.stderr
        );

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
    fn clap_tree_is_valid() {
        Cli::command().debug_assert();
    }
}
