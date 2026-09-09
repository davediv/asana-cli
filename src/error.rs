//! Turn every failure into the one- or two-line messages gh prints, and pick
//! the exit code.
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
//! Exit codes are gh's: 0 ok, 1 error (including usage), 2 cancelled by the
//! user, 4 authentication required or rejected.

use std::error::Error as _;
use std::ffi::OsString;

use clap::error::{ContextKind, ContextValue, ErrorKind};

use crate::api::ApiError;
use crate::cli::{self, Cancelled, Rendered, SilentExit, UsageError};
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
            format!("{}\n\n{}", usage.message, cli::usage_block(&usage.path)),
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
/// says exactly what to set; an unreadable or malformed file is exit 1.
pub fn report_config(err: &ConfigError) -> Report {
    let code = match err {
        ConfigError::MissingToken(_) => EXIT_AUTH,
        _ => EXIT_ERROR,
    };
    Report::line(err.to_string(), code)
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
        other => Report::line(other.to_string(), EXIT_ERROR),
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

/// Render a clap error. `cli::render_clap_error` already speaks cobra for
/// unknown commands, unknown flags, missing arguments and the rest; this
/// only rewrites the reason under a flag value that failed to parse, so
/// `--limit abc` reads `expected a whole number` instead of Rust's
/// `invalid digit found in string`.
pub fn render_clap_error(err: &clap::Error, argv: &[OsString], palette: &Palette) -> Rendered {
    if let Some(rendered) = render_bad_flag_value(err, argv) {
        return rendered;
    }
    cli::render_clap_error(err, argv, palette)
}

fn render_bad_flag_value(err: &clap::Error, argv: &[OsString]) -> Option<Rendered> {
    if err.kind() != ErrorKind::ValueValidation {
        return None;
    }
    let reason = err
        .source()
        .and_then(|s| s.downcast_ref::<std::num::ParseIntError>())
        .map(int_reason)?;
    let arg = context_string(err, ContextKind::InvalidArg)?;
    let arg = arg.split_whitespace().next().unwrap_or(&arg).to_string();
    if !arg.starts_with('-') {
        return None;
    }
    let value = context_string(err, ContextKind::InvalidValue).unwrap_or_default();
    let root = cli::command();
    let path = cli::command_path(&root, argv);
    Some(Rendered {
        stdout: String::new(),
        stderr: format!(
            "invalid argument \"{value}\" for \"{arg}\" flag: {reason}\n\n{}",
            cli::usage_block(&path)
        ),
        code: EXIT_ERROR,
    })
}

fn int_reason(err: &std::num::ParseIntError) -> &'static str {
    use std::num::IntErrorKind;
    match err.kind() {
        IntErrorKind::PosOverflow | IntErrorKind::NegOverflow => "number is too large",
        _ => "expected a whole number",
    }
}

fn context_string(err: &clap::Error, kind: ContextKind) -> Option<String> {
    match err.get(kind) {
        Some(ContextValue::String(s)) => Some(s.clone()),
        Some(ContextValue::Strings(v)) => v.first().cloned(),
        Some(ContextValue::StyledStr(s)) => Some(s.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
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
        let err = cli::usage_error(&["task", "create"], "title cannot be blank");
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
        use clap::Parser as _;
        cli::Cli::try_parse_from(argv(parts)).expect_err("should fail to parse")
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
