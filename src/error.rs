//! Turn every failure into the one- or two-line messages gh prints, and pick
//! the exit code. Every piece of error text lives here: the runtime
//! reports, and the clap/usage errors (`unknown command`, `unknown flag`,
//! `accepts N arg(s), received M`, `flag needs an argument`, the `Usage:` +
//! `Flags:` block cobra prints under them).
//!
//! The shapes come from `reference/gh-errors` and `reference/stripe-errors`:
//!
//! - runtime errors are one line, no stack trace, no usage dump:
//!   `HTTP 404: task: Unknown object: 1 (https://app.asana.com/api/1.0/tasks/1)`
//! - a rejected token gets a second line saying what to try, like gh's
//!   `Try authenticating with:  gh auth login -h github.com`
//! - usage errors are the message, a blank line, then `Usage:` and the flag
//!   table (cobra's behaviour, which gh inherits)
//!
//! `cli.rs` (the help piece) owns the command tree and the help screens;
//! this file borrows its flag-table formatting for the `Usage:` block so
//! the two never drift apart.
//!
//! Exit codes are gh's: 0 ok, 1 error (including usage), 2 cancelled by the
//! user, 4 authentication required or rejected.

use std::error::Error as _;
use std::ffi::OsString;
use std::fmt;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::Command;

use crate::api::ApiError;
use crate::cli;
use crate::config::{ConfigError, TokenSource, PAT_URL};
use crate::render::Palette;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_CANCELLED: i32 = 2;
pub const EXIT_AUTH: i32 = 4;

/// What goes to stderr (already newline-terminated, possibly empty) and the
/// process exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub stderr: String,
    pub code: i32,
}

impl Report {
    fn line(text: impl Into<String>, code: i32) -> Self {
        let mut stderr = text.into();
        if !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        Self { stderr, code }
    }

    fn silent(code: i32) -> Self {
        Self {
            stderr: String::new(),
            code,
        }
    }
}

/// The first error of type `T` anywhere in the chain, so a typed error still
/// picks its exit code after someone wrapped it with `.context(..)`.
fn find<T: std::error::Error + 'static>(err: &anyhow::Error) -> Option<&T> {
    err.chain().find_map(|e| e.downcast_ref::<T>())
}

/// Render a runtime error. `token_source` is where the token came from, so a
/// 401 can say which variable or file to fix.
pub fn report(err: &anyhow::Error, token_source: Option<&TokenSource>) -> Report {
    if let Some(usage) = find::<UsageError>(err) {
        return Report::line(
            format!("{}\n\n{}", usage.message, usage_block(&usage.path)),
            EXIT_ERROR,
        );
    }
    if find::<Cancelled>(err).is_some() {
        return Report::silent(EXIT_CANCELLED);
    }
    if let Some(silent) = find::<SilentExit>(err) {
        return Report::silent(silent.0);
    }
    if let Some(api) = find::<ApiError>(err) {
        return report_api(api, token_source);
    }
    if let Some(cfg) = find::<ConfigError>(err) {
        return report_config(cfg);
    }
    Report::line(format!("{err:#}"), EXIT_ERROR)
}

/// Render a configuration error: no token is an auth error (exit 4) that
/// says exactly what to set; an unreadable or malformed file is exit 1 with
/// its cause after a colon.
pub fn report_config(err: &ConfigError) -> Report {
    let code = match err {
        ConfigError::MissingToken(_) => EXIT_AUTH,
        _ => EXIT_ERROR,
    };
    Report::line(chain(err), code)
}

/// `message: cause: cause` down the `#[source]` chain, the way anyhow's
/// `{:#}` prints it, for errors that are not wrapped in anyhow.
fn chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(next) = cur {
        out.push_str(": ");
        out.push_str(&next.to_string());
        cur = next.source();
    }
    out
}

/// Render an API error the way gh renders an HTTP error.
pub fn report_api(err: &ApiError, token_source: Option<&TokenSource>) -> Report {
    match err {
        ApiError::Unauthorized { .. } => {
            Report::line(format!("{err}\n{}", auth_hint(token_source)), EXIT_AUTH)
        }
        ApiError::Network(source) => Report::line(
            format!(
                "error connecting to {}: {}\ncheck your internet connection or https://status.asana.com",
                host_of(source),
                network_reason(source)
            ),
            EXIT_ERROR,
        ),
        other => Report::line(chain(other), EXIT_ERROR),
    }
}

/// The second line under a 401: where the rejected token came from and where
/// to get a new one (gh: `Try authenticating with:  gh auth login -h github.com`).
fn auth_hint(source: Option<&TokenSource>) -> String {
    let place = match source {
        Some(TokenSource::Env(name)) => (*name).to_string(),
        Some(TokenSource::ConfigFile(path)) => crate::config::tilde(path),
        None => crate::config::TOKEN_ENV.to_string(),
    };
    format!("Check the token in {place}, or create a new one at {PAT_URL}")
}

/// The host we failed to reach: from the request URL when reqwest kept it,
/// else the production host.
fn host_of(err: &reqwest::Error) -> String {
    err.url()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "app.asana.com".to_string())
}

/// The innermost reason of a reqwest error, without the "error sending
/// request for url" wrapper and without the `(os error N)` suffix.
fn network_reason(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        return "timed out".to_string();
    }
    let mut cur: &dyn std::error::Error = err;
    while let Some(next) = cur.source() {
        cur = next;
    }
    strip_os_error(&cur.to_string())
}

fn strip_os_error(reason: &str) -> String {
    match reason.rfind(" (os error ") {
        Some(pos) if reason.ends_with(')') => reason[..pos].to_string(),
        _ => reason.to_string(),
    }
}

// ---------------------------------------------------------------- usage errors

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

/// Shared helper for command modules: a usage error for the command at
/// `path` (e.g. `&["task", "create"]`), as an `anyhow::Error`.
pub fn usage_error(path: &[&str], message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError::new(path, message))
}

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

/// What to print (stdout, stderr) and the exit code for a clap error or a
/// built-in request (`asana help`, `asana version`).
pub struct Rendered {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// The `Usage:` + `Flags:` (or `Available commands:`) block cobra prints
/// under usage errors, for the command at `path`.
pub fn usage_block(path: &[String]) -> String {
    let root = cli::command();
    let mut out = format!("Usage:  {}\n", cli::usage_line(path));
    if let Some(cmd) = cli::find(&root, path) {
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
                cli::root_flags(cmd)
            } else {
                cli::local_flags(cmd)
            };
            let lines = cli::flag_lines(flags);
            if !lines.is_empty() {
                out.push_str("\nFlags:\n");
                out.push_str(&cli::render_flag_table(&lines));
                out.push_str("  \n");
            }
        }
    }
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

/// cobra's "Did you mean this?" candidates: within two edits, or a prefix.
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

/// `unknown command "lsit" for "asana task"` + suggestions + the command list.
fn unknown_command(typed: &str, root: &Command, path: &[String]) -> Rendered {
    let parent = std::iter::once("asana".to_string())
        .chain(path.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut msg = format!("unknown command \"{typed}\" for \"{parent}\"\n");
    let names: Vec<String> = cli::find(root, path)
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
        code: EXIT_ERROR,
    }
}

/// Why a flag value failed to parse, in pflag's words: `--limit abc` reads
/// `expected a whole number` rather than Rust's `invalid digit found in
/// string`.
fn value_reason(err: &clap::Error) -> String {
    let Some(source) = err.source() else {
        return "invalid syntax".to_string();
    };
    match source.downcast_ref::<std::num::ParseIntError>() {
        Some(int) => int_reason(int).to_string(),
        None => source.to_string(),
    }
}

fn int_reason(err: &std::num::ParseIntError) -> &'static str {
    use std::num::IntErrorKind;
    match err.kind() {
        IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => "number is too large",
        _ => "expected a whole number",
    }
}

/// Map a clap error to gh's wording. Help and version go to stdout with exit
/// 0; usage errors go to stderr with exit 1. `asana help ...` and `asana
/// version`, which clap reports as unknown subcommands, are handed to the
/// help piece (`cli::builtin_request`).
pub fn render_clap_error(err: &clap::Error, argv: &[OsString], palette: &Palette) -> Rendered {
    let root = cli::command();
    let path = cli::command_path(&root, argv);
    let ok = |stdout: String| Rendered {
        stdout,
        stderr: String::new(),
        code: EXIT_OK,
    };
    let usage = |message: String, path: &[String]| Rendered {
        stdout: String::new(),
        stderr: format!("{message}\n\n{}", usage_block(path)),
        code: EXIT_ERROR,
    };
    let bare = |message: String| Rendered {
        stdout: String::new(),
        stderr: format!("{message}\n"),
        code: EXIT_ERROR,
    };
    if matches!(
        err.kind(),
        ErrorKind::InvalidSubcommand | ErrorKind::UnknownArgument
    ) {
        if let Some(rendered) = cli::builtin_request(&root, argv, palette) {
            return rendered;
        }
    }
    match err.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        | ErrorKind::MissingSubcommand => ok(cli::render_help(&path, palette)),
        ErrorKind::DisplayVersion => ok(cli::render_version()),
        ErrorKind::InvalidSubcommand => unknown_command(
            &context_string(err, ContextKind::InvalidSubcommand).unwrap_or_default(),
            &root,
            &path,
        ),
        ErrorKind::UnknownArgument => {
            let arg = context_string(err, ContextKind::InvalidArg).unwrap_or_default();
            let leaf = cli::find(&root, &path);
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
            let leaf = cli::find(&root, &path);
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
                usage(
                    format!(
                        "invalid argument \"{value}\" for \"{arg}\" flag: {}",
                        value_reason(err)
                    ),
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
            let (n, _) = cli::find(&root, &path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use clap::Parser as _;
    use std::path::PathBuf;

    fn env_source() -> TokenSource {
        TokenSource::Env("ASANA_TOKEN")
    }

    #[test]
    fn plain_errors_are_one_line_exit_1() {
        let r = report(&anyhow::anyhow!("invalid task format: \"abc\""), None);
        assert_eq!(r.stderr, "invalid task format: \"abc\"\n");
        assert_eq!(r.code, EXIT_ERROR);
    }

    #[test]
    fn context_chains_read_like_gh() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory");
        let err = Err::<(), _>(io)
            .context("could not read notes.md")
            .unwrap_err();
        let r = report(&err, None);
        assert_eq!(
            r.stderr,
            "could not read notes.md: No such file or directory\n"
        );
        assert_eq!(r.code, EXIT_ERROR);
    }

    #[test]
    fn unauthorized_exits_4_and_names_the_token_source() {
        let err = ApiError::Unauthorized {
            message: "Not Authorized".into(),
            url: "https://app.asana.com/api/1.0/users/me".into(),
        };
        let r = report_api(&err, Some(&env_source()));
        assert_eq!(
            r.stderr,
            "HTTP 401: Not Authorized (https://app.asana.com/api/1.0/users/me)\n\
             Check the token in ASANA_TOKEN, or create a new one at https://app.asana.com/0/my-apps\n"
        );
        assert_eq!(r.code, EXIT_AUTH);

        let file = TokenSource::ConfigFile(PathBuf::from("/nonexistent/asana/config.toml"));
        let r = report_api(&err, Some(&file));
        assert!(r
            .stderr
            .contains("Check the token in /nonexistent/asana/config.toml, or create"));

        let r = report_api(&err, Some(&TokenSource::Env("ASANA_ACCESS_TOKEN")));
        assert!(r
            .stderr
            .contains("Check the token in ASANA_ACCESS_TOKEN, or"));

        let r = report_api(&err, None);
        assert!(r.stderr.contains("Check the token in ASANA_TOKEN, or"));
    }

    #[test]
    fn unauthorized_is_found_through_context() {
        let err = anyhow::Error::new(ApiError::Unauthorized {
            message: "Not Authorized".into(),
            url: "u".into(),
        })
        .context("could not list tasks");
        assert_eq!(report(&err, None).code, EXIT_AUTH);
    }

    #[test]
    fn not_found_is_the_api_message_with_status_and_url() {
        let err = ApiError::NotFound {
            message: "task: Unknown object: 1".into(),
            url: "https://app.asana.com/api/1.0/tasks/1".into(),
        };
        let r = report_api(&err, None);
        assert_eq!(
            r.stderr,
            "HTTP 404: task: Unknown object: 1 (https://app.asana.com/api/1.0/tasks/1)\n"
        );
        assert_eq!(r.code, EXIT_ERROR);
    }

    #[test]
    fn other_statuses_keep_gh_shape() {
        let err = ApiError::Status {
            status: 500,
            message: "Server Error".into(),
            url: "https://app.asana.com/api/1.0/tasks".into(),
        };
        assert_eq!(
            report_api(&err, None).stderr,
            "HTTP 500: Server Error (https://app.asana.com/api/1.0/tasks)\n"
        );
        let err = ApiError::Forbidden {
            message: "Forbidden".into(),
            url: "https://app.asana.com/api/1.0/tasks/9".into(),
        };
        let r = report_api(&err, None);
        assert_eq!(
            r.stderr,
            "HTTP 403: Forbidden (https://app.asana.com/api/1.0/tasks/9)\n"
        );
        assert_eq!(r.code, EXIT_ERROR);
        let err = ApiError::RateLimited { retry_after: 7 };
        assert_eq!(
            report_api(&err, None).stderr,
            "HTTP 429: Rate Limit Enforced; try again in 7s\n"
        );
    }

    #[test]
    fn cancelled_and_silent_print_nothing() {
        let r = report(&anyhow::Error::new(Cancelled), None);
        assert_eq!(r, Report::silent(EXIT_CANCELLED));
        let r = report(&anyhow::Error::new(SilentExit(4)), None);
        assert_eq!(r, Report::silent(4));
    }

    #[test]
    fn usage_errors_print_message_then_usage_block() {
        let err = usage_error(&["task", "create"], "title cannot be blank");
        let r = report(&err, None);
        assert!(r
            .stderr
            .starts_with("title cannot be blank\n\nUsage:  asana task create [flags]\n\nFlags:\n"));
        assert!(r.stderr.ends_with("  \n"));
        assert_eq!(r.code, EXIT_ERROR);
    }

    #[test]
    fn missing_token_exits_4() {
        let err = ConfigError::MissingToken("~/.config/asana/config.toml".into());
        let r = report_config(&err);
        assert_eq!(r.code, EXIT_AUTH);
        assert!(r.stderr.starts_with(
            "To get started with the Asana CLI, populate the ASANA_TOKEN environment variable"
        ));
        assert_eq!(r.stderr.lines().count(), 2);
        let r = report(&anyhow::Error::new(err), None);
        assert_eq!(r.code, EXIT_AUTH);
    }

    #[test]
    fn os_error_suffix_is_dropped() {
        assert_eq!(
            strip_os_error("Connection refused (os error 61)"),
            "Connection refused"
        );
        assert_eq!(strip_os_error("timed out"), "timed out");
        assert_eq!(strip_os_error("odd (os error"), "odd (os error");
    }

    fn argv(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    fn parse_failure(parts: &[&str]) -> clap::Error {
        cli::Cli::try_parse_from(argv(parts)).expect_err("should fail to parse")
    }

    #[test]
    fn config_file_errors_show_the_cause_once() {
        let err = ConfigError::Unreadable {
            path: PathBuf::from("/x/config.toml"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"),
        };
        assert_eq!(
            report_config(&err).stderr,
            "could not read /x/config.toml: Permission denied\n"
        );
        let r = report(&anyhow::Error::new(err), None);
        assert_eq!(
            r.stderr,
            "could not read /x/config.toml: Permission denied\n"
        );
        assert_eq!(r.code, EXIT_ERROR);
    }

    #[test]
    fn decode_errors_show_the_cause_once() {
        let source = serde_json::from_str::<serde_json::Value>("{nope").unwrap_err();
        let err = ApiError::Decode {
            url: "https://app.asana.com/api/1.0/tasks".into(),
            source,
        };
        let r = report_api(&err, None);
        assert!(
            r.stderr.starts_with("could not parse the response from https://app.asana.com/api/1.0/tasks: key must be a string"),
            "{}",
            r.stderr
        );
        assert_eq!(r.stderr.matches("could not parse").count(), 1);
    }

    #[test]
    fn bad_int_flag_value_reads_like_pflag() {
        let args = argv(&["asana", "task", "list", "--limit", "abc"]);
        let err = parse_failure(&["asana", "task", "list", "--limit", "abc"]);
        let r = render_clap_error(&err, &args, &Palette::off());
        assert_eq!(r.code, EXIT_ERROR);
        assert!(r.stdout.is_empty());
        assert!(
            r.stderr.starts_with(
                "invalid argument \"abc\" for \"--limit\" flag: expected a whole number\n\nUsage:  asana task list [flags]\n\nFlags:\n"
            ),
            "{}",
            r.stderr
        );
        assert!(r.stderr.contains("-L, --limit int"));

        let big = "99999999999999999999999";
        let args = argv(&["asana", "project", "list", "--limit", big]);
        let err = parse_failure(&["asana", "project", "list", "--limit", big]);
        let r = render_clap_error(&err, &args, &Palette::off());
        assert!(r.stderr.starts_with(&format!(
            "invalid argument \"{big}\" for \"--limit\" flag: number is too large\n"
        )));
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
            let err = cli::Cli::try_parse_from(&args).expect_err("clap reports these");
            let r = render_clap_error(&err, &args, &p);
            assert_eq!(r.code, 0, "{parts:?}");
            assert!(r.stderr.is_empty(), "{parts:?}");
            assert!(!r.stdout.is_empty(), "{parts:?}");
        }
        let args = argv(&["asana", "task", "help"]);
        let err = cli::Cli::try_parse_from(&args).expect_err("unknown subcommand");
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
    fn positional_and_flag_errors_read_like_cobra() {
        let p = Palette::off();
        let render = |parts: &[&str]| {
            let args = argv(parts);
            let err = parse_failure(parts);
            render_clap_error(&err, &args, &p)
        };
        assert_eq!(
            render(&["asana", "task", "view", "1", "2"]).stderr,
            "accepts 1 arg(s), received 2\n"
        );
        assert_eq!(
            render(&["asana", "task", "complete"]).stderr,
            "requires at least 1 arg(s), only received 0\n"
        );
        assert_eq!(
            render(&["asana", "task", "assign"]).stderr,
            "accepts 2 arg(s), received 0\n"
        );
        let r = render(&["asana", "task", "create", "--title"]);
        assert!(r.stderr.starts_with(
            "flag needs an argument: --title\n\nUsage:  asana task create [flags]\n\nFlags:\n"
        ));
        let r = render(&[
            "asana",
            "task",
            "comment",
            "1",
            "--body",
            "x",
            "--body-file",
            "y",
        ]);
        assert!(r.stderr.starts_with(
            "specify only one of `--body` or `--body-file`\n\nUsage:  asana task comment"
        ));
        let r = render(&["asana", "task", "list", "--state", "nope"]);
        assert!(r.stderr.starts_with(
            "invalid argument \"nope\" for \"--state\" flag: must be one of open, completed, all\n"
        ), "{}", r.stderr);
        let r = render(&["asana", "nope"]);
        assert!(r.stderr.starts_with(
            "unknown command \"nope\" for \"asana\"\n\nUsage:  asana <command> <subcommand> [flags]\n\nAvailable commands:\n  auth\n"
        ), "{}", r.stderr);
    }

    #[test]
    fn other_clap_errors_are_delegated() {
        let args = argv(&["asana", "task", "list", "--bogus"]);
        let err = parse_failure(&["asana", "task", "list", "--bogus"]);
        let r = render_clap_error(&err, &args, &Palette::off());
        assert!(r
            .stderr
            .starts_with("unknown flag: --bogus\n\nUsage:  asana task list [flags]\n"));
        assert_eq!(r.code, EXIT_ERROR);

        let args = argv(&["asana", "task", "view"]);
        let err = parse_failure(&["asana", "task", "view"]);
        let r = render_clap_error(&err, &args, &Palette::off());
        assert_eq!(r.stderr, "accepts 1 arg(s), received 0\n");

        let args = argv(&["asana", "task", "lsit"]);
        let err = parse_failure(&["asana", "task", "lsit"]);
        let r = render_clap_error(&err, &args, &Palette::off());
        assert!(r
            .stderr
            .starts_with("unknown command \"lsit\" for \"asana task\"\n\nDid you mean this?\n\tlist\n\nUsage:  asana task <command> [flags]\n"));
    }
}
