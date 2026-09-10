//! `asana auth login`, `asana auth logout` and `asana auth token`, laid out
//! like `gh auth login`, `gh auth logout` and `gh auth token`.
//!
//! All three run before a token is required, so they never fail with the
//! "set ASANA_TOKEN" error. `login` takes a personal access token from a
//! prompt or, with `--with-token`, from standard input, checks it against
//! `/users/me`, and stores it through `config::save_login` (mode 600, other
//! keys and comments kept); `logout` removes it through
//! `config::remove_token`; `token` prints whichever token every other
//! command would use.
//!
//! What each prints, and the references it is judged against:
//!
//! ```text
//! $ echo $TOKEN | asana auth login --with-token
//! ✓ Logged in to app.asana.com account Dave Div (~/.config/asana/config.toml)
//!
//! $ echo notarealtoken | asana auth login --with-token      (gh-errors/auth-login-bad-token)
//! HTTP 401: the token was rejected by app.asana.com, so it was not stored.
//! Check it, or create a new one at https://app.asana.com/0/my-apps.
//! [exit 4]
//!
//! $ asana auth login < /dev/null                            (gh-errors/create-noninteractive)
//! Cannot prompt for a token when not running interactively.
//! Pipe the token in: echo "$TOKEN" | asana auth login --with-token
//!
//! Usage:  asana auth login [flags]
//! ...
//! [exit 1]
//!
//! $ asana auth logout
//! ✓ Logged out of app.asana.com account Dave Div
//!
//! $ asana auth logout                                       (gh-errors/auth-logout-none)
//! You are not logged in to app.asana.com. To log in, run: asana auth login
//! [exit 1]
//!
//! $ asana auth token                                        (gh-errors/auth-status-none)
//! You are not logged in to app.asana.com. To log in, run: asana auth login
//! [exit 1]
//! ```
//!
//! # One voice for every failure
//!
//! gh speaks three ways here: `error validating token: HTTP 401: ...` for
//! a bad token, the bare `not logged in to any hosts` for a logout with
//! nothing stored, and the sentence `You are not logged into any GitHub
//! hosts. To log in, run: gh auth login` for status. Every failure in this
//! file uses the one shape `error.rs` gives the rest of the CLI instead: a
//! sentence saying what happened (an API error opens with gh's `HTTP
//! <status>:` label), then a line saying what to do next, which names the
//! command to run after `run:` or `Pipe the token in:` where there is one.
//! No `error:` prefix, no lowercase fragment, nothing wider than 79
//! columns, and the not-logged-in state is one sentence wherever it is met.
//! A usage error (the prompt without a terminal) keeps the `Usage:` block
//! under its two lines, as every usage error in the CLI does.
//!
//! # Exit codes
//!
//! `asana help exit-codes` gives 4 to a command that requires
//! authentication and 1 to every other failure. A token Asana rejects is
//! an authentication failure, so `login` exits 4 with it: the same code
//! `task list` exits with when its token is rejected, so a script can tell
//! a bad credential from a bad invocation. Not being logged in is an
//! ordinary error (1) for `logout` and `token`: neither needs a token to
//! run, they manage the one that is stored, and a logout that removed
//! nothing did not do what was asked. gh draws the line in the same place
//! (`auth logout` and `auth token` exit 1 with nothing stored). A script
//! that wants a no-op logout writes `asana auth logout 2>/dev/null ||
//! true`, the way it would for `rm`.
//!
//! The account name is kept beside the token (`user = "..."`) so `logout`
//! can say whose token it removed, the way `gh auth logout` names the
//! account, without a network call.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{anyhow, Context, Result};

use crate::api::{self, ApiError, User, USER_FIELDS};
use crate::cli::{usage_error, Cancelled, SilentExit};
use crate::cmd::{confirm, Prevalidate};
use crate::config::{
    base_url, host, load, remove_token, save_login, stored_login, tilde, ConfigError, PAT_URL,
    TOKEN_ENV, TOKEN_ENV_ALT,
};
use crate::render::Palette;

const EXIT_ERROR: i32 = 1;
const EXIT_AUTH: i32 = 4;

// ------------------------------------------------------------------- help

/// The description explains the two ways to authenticate, in gh's order: the
/// stored token (prompt or `--with-token`), then the environment variable
/// for automation. Hand-wrapped short of 78 columns like `cmd::help`'s
/// prose; the trailing newline leaves gh's two blank lines before USAGE.
const LOGIN_LONG_ABOUT: &str = "\
Authenticate with Asana.

The default authentication mode is an interactive prompt for a personal
access token. After completion, the token is stored in the config file
(~/.config/asana/config.toml), readable only by you, where every other
command finds it. Run `asana auth status` to see which account it belongs to.

Alternatively, use `--with-token` to pass in a personal access token on
standard input. Create one at https://app.asana.com/0/my-apps.

Alternatively, asana will use the authentication token found in the
ASANA_TOKEN or ASANA_ACCESS_TOKEN environment variable. This method is most
suitable for \"headless\" use of asana such as in automation, and takes
precedence over the config file. See `asana help environment` for more info.
";

const LOGIN_EXAMPLES: &str = "\
# Start interactive setup
$ asana auth login

# Authenticate by reading the token from a file
$ asana auth login --with-token < mytoken.txt

# Authenticate from a script, then check who you are
$ echo \"$TOKEN\" | asana auth login --with-token && asana auth status";

const LOGOUT_LONG_ABOUT: &str = "\
Remove authentication for an Asana account.

This command removes the stored token from the config file
(~/.config/asana/config.toml). The token is only removed locally.

This command does not revoke the token. A token set through the ASANA_TOKEN
or ASANA_ACCESS_TOKEN environment variable is not touched either; unset the
variable to stop using it.

To revoke a personal access token:

1. Visit https://app.asana.com/0/my-apps
2. Open \"Personal access tokens\"
3. Delete the token

Note: this procedure revokes the token everywhere it is in use, not only on
this device.
";

const LOGOUT_EXAMPLES: &str = "\
# Remove the stored token
$ asana auth logout";

const TOKEN_LONG_ABOUT: &str = "\
This command outputs the authentication token asana uses.

The token comes from the ASANA_TOKEN environment variable, then
ASANA_ACCESS_TOKEN, then `token` in the config file
(~/.config/asana/config.toml), in that order, so the output is exactly what
every other command sends to Asana. Use it to hand the same credential to
other tools.
";

const TOKEN_EXAMPLES: &str = "\
# Call the Asana API with the token asana uses
$ curl -H \"Authorization: Bearer $(asana auth token)\" https://app.asana.com/api/1.0/users/me";

#[derive(clap::Args, Debug)]
#[command(
    about = "Log in to an Asana account",
    long_about = LOGIN_LONG_ABOUT,
    after_help = LOGIN_EXAMPLES
)]
pub struct LoginArgs {
    /// Read token from standard input
    #[arg(long)]
    pub with_token: bool,
}

/// gh's own check: a prompt needs a terminal on every stream, so a script
/// must pass `--with-token`. A usage error (the two lines, then the
/// `Usage:` block), exit 1, before the token is read.
impl Prevalidate for LoginArgs {
    fn prevalidate(&self, interactive: bool) -> Result<()> {
        if !self.with_token && !interactive {
            return Err(usage_error(&["auth", "login"], cannot_prompt()));
        }
        Ok(())
    }
}

#[derive(clap::Args, Debug)]
#[command(
    about = "Log out of an Asana account",
    long_about = LOGOUT_LONG_ABOUT,
    after_help = LOGOUT_EXAMPLES
)]
pub struct LogoutArgs {}
impl Prevalidate for LogoutArgs {}

#[derive(clap::Args, Debug)]
#[command(
    about = "Print the authentication token asana uses",
    long_about = TOKEN_LONG_ABOUT,
    after_help = TOKEN_EXAMPLES
)]
pub struct TokenArgs {}
impl Prevalidate for TokenArgs {}

// ---------------------------------------------------------------- commands

/// `asana auth login`: take the token, check it, store it, say who it is.
pub async fn run_login(args: LoginArgs, palette: &Palette) -> Result<()> {
    let token = if args.with_token {
        read_token_from_stdin()?
    } else {
        match prompt_for_token()? {
            Some(token) => token,
            // Declined to replace the stored token: nothing to do, like gh.
            None => return Ok(()),
        }
    };
    let user = validate(&token).await?;
    let name = account_name(&user);
    let path = save_login(&token, Some(name.as_str()))?;
    eprintln!(
        "{} Logged in to {} account {} ({})",
        palette.success_icon(),
        host(),
        palette.bold(&name),
        tilde(&path)
    );
    if let Some(var) = env_token_var() {
        eprintln!(
            "{} {var} is set and takes precedence over the stored token",
            palette.warning_icon()
        );
    }
    Ok(())
}

/// `asana auth logout`: remove the stored token, naming the account it
/// belonged to. With nothing stored there is nothing to do: the
/// not-logged-in line (or, when a variable is the login, what to unset)
/// and exit 1, like gh.
pub async fn run_logout(_args: LogoutArgs, palette: &Palette) -> Result<()> {
    let Some(stored) = stored_login()? else {
        return Err(nothing_to_log_out(palette));
    };
    if !remove_token()? {
        // Gone between the read and the write: nothing to remove after all.
        return Err(nothing_to_log_out(palette));
    }
    let host = host();
    match stored.user {
        Some(user) => eprintln!(
            "{} Logged out of {host} account {}",
            palette.success_icon(),
            palette.bold(&user)
        ),
        None => eprintln!("{} Logged out of {host}", palette.success_icon()),
    }
    if let Some(var) = env_token_var() {
        eprintln!(
            "{} {var} is still set and will be used for authentication",
            palette.warning_icon()
        );
    }
    Ok(())
}

/// `asana auth token`: the token every other command uses, alone on stdout
/// so it can be substituted into another command. Without one, the same
/// not-logged-in line as `logout`, exit 1.
pub async fn run_token(_args: TokenArgs, palette: &Palette) -> Result<()> {
    match load(None) {
        Ok(cfg) => {
            println!("{}", cfg.token);
            Ok(())
        }
        Err(ConfigError::MissingToken(_)) => {
            eprintln!("{}", not_logged_in(palette));
            Err(SilentExit(EXIT_ERROR).into())
        }
        Err(err) => Err(err.into()),
    }
}

// --------------------------------------------------------------- messages
//
// Every failure below is a sentence saying what happened, then a line
// saying what to do next; a command to run follows `run:` (or `Pipe the
// token in:`) after one space. Runtime errors that exit 1 are returned as
// plain errors, so `error::report` prints them as they are (each line is
// kept inside its 79 columns by hand); the two that paint or exit 4 are
// printed here and finished with `SilentExit`.

/// The next step for a script: the token on standard input.
const PIPE_HINT: &str = "Pipe the token in: echo \"$TOKEN\" | asana auth login --with-token";

/// The one sentence for the not-logged-in state, shared by `logout` and
/// `token`: gh's `You are not logged into any GitHub hosts. To log in, run:
/// gh auth login`, naming the host we would have asked, with the command in
/// bold as gh paints it.
fn not_logged_in(palette: &Palette) -> String {
    format!(
        "You are not logged in to {}. To log in, run: {}",
        host(),
        palette.bold("asana auth login")
    )
}

/// `logout` found no stored token. Nothing was removed, so this is an
/// error (exit 1, where gh prints `not logged in to any hosts`): the
/// not-logged-in line, or, when an environment variable is what every
/// command authenticates with, the variable to unset instead of a login to
/// run.
fn nothing_to_log_out(palette: &Palette) -> anyhow::Error {
    match env_token_var() {
        Some(var) => anyhow!("{}", logout_leaves_env(var)),
        None => {
            eprintln!("{}", not_logged_in(palette));
            SilentExit(EXIT_ERROR).into()
        }
    }
}

/// The login is `var`, which `logout` cannot remove: what it is, and how
/// to stop using it.
fn logout_leaves_env(var: &str) -> String {
    format!(
        "The token comes from {var}, which logout does not touch.\n\
         To stop using it, run: unset {var}"
    )
}

/// `login` without `--with-token` and without a terminal to prompt on:
/// the reason, then the way in for a script.
fn cannot_prompt() -> String {
    format!("Cannot prompt for a token when not running interactively.\n{PIPE_HINT}")
}

/// `--with-token` read an empty line: the same next step.
fn no_token_on_stdin() -> anyhow::Error {
    anyhow!("No token was read from standard input.\n{PIPE_HINT}")
}

/// Asana answered 401 to the candidate token. The lines `error.rs` prints
/// for a rejected token, adapted to one that was never stored: the `HTTP
/// 401:` label opens the sentence, which says the token went nowhere, and
/// the next step is the token itself (`asana auth login` is what the user
/// just ran, so it is not repeated). gh's `error validating token:` prefix
/// and the endpoint that saw the 401 are left out: a rejected token is
/// rejected everywhere.
fn token_rejected() -> String {
    format!(
        "HTTP 401: the token was rejected by {}, so it was not stored.\n\
         Check it, or create a new one at {PAT_URL}.",
        host()
    )
}

// ------------------------------------------------------------- validation

/// `GET /users/me` with the candidate token. A 401 is `token_rejected`,
/// exit 4 (authentication rejected, the code every command exits with for
/// a bad token); anything else (network, 5xx) is reported the way every
/// other command reports it.
async fn validate(token: &str) -> Result<User> {
    let client = match base_url() {
        Some(base) => api::Client::with_base(&base, token),
        None => api::Client::new(token),
    }
    .context("could not create the HTTP client")?;
    match client
        .get::<User>("/users/me", &[("opt_fields", USER_FIELDS)])
        .await
    {
        Ok(user) => Ok(user),
        Err(ApiError::Unauthorized { .. }) => {
            eprintln!("{}", token_rejected());
            Err(SilentExit(EXIT_AUTH).into())
        }
        Err(err) => Err(err.into()),
    }
}

/// The name to call the account by: the display name, else the email, else
/// the gid.
fn account_name(user: &User) -> String {
    fn non_empty(s: &Option<String>) -> Option<&str> {
        s.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    non_empty(&user.name)
        .or_else(|| non_empty(&user.email))
        .unwrap_or(&user.gid)
        .to_string()
}

/// The environment variable a token is being read from, if any: it wins
/// over the stored token, which is worth saying when storing or removing
/// one.
fn env_token_var() -> Option<&'static str> {
    [TOKEN_ENV, TOKEN_ENV_ALT]
        .into_iter()
        .find(|var| std::env::var(var).is_ok_and(|v| !v.trim().is_empty()))
}

// ------------------------------------------------------------------ input

/// `--with-token`: the first line of standard input, trimmed, like
/// `gh auth login --with-token < mytoken.txt`.
fn read_token_from_stdin() -> Result<String> {
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .context("could not read the token from standard input")?;
    let token = line.trim().to_string();
    if token.is_empty() {
        return Err(no_token_on_stdin());
    }
    Ok(token)
}

/// The interactive path, shaped like gh's "Paste an authentication token"
/// flow: an "already logged in" question when a token is stored, the tip
/// with the token page, then the hidden prompt. `None` when the user keeps
/// the stored token.
fn prompt_for_token() -> Result<Option<String>> {
    if let Some(stored) = stored_login()? {
        let who = stored.user.map(|u| format!(" as {u}")).unwrap_or_default();
        let question = format!(
            "You're already logged in to {}{who}. Do you want to re-authenticate?",
            host()
        );
        if !confirm(&question)? {
            return Ok(None);
        }
    }
    eprintln!("Tip: you can generate a personal access token here {PAT_URL}");
    loop {
        eprint!("? Paste your authentication token: ");
        io::stderr().flush().ok();
        let Some(line) = read_hidden_line()? else {
            eprintln!();
            return Err(Cancelled.into());
        };
        let token = line.trim().to_string();
        eprintln!("{}", "*".repeat(token.chars().count().min(40)));
        if token.is_empty() {
            eprintln!("Sorry, your reply was invalid: a token is required");
            continue;
        }
        return Ok(Some(token));
    }
}

/// One line from the terminal with echo off, as a password prompt reads.
/// `None` on end of input or Ctrl-C.
///
/// The read is delegated to `sh`, which turns echo off with `stty`, reads
/// the line, and turns echo back on, and whose `trap` restores echo on
/// Ctrl-C before the shell exits 130, so the terminal is never left without
/// echo. This process only needs to survive that Ctrl-C (the signal reaches
/// the whole foreground group), so it installs a no-op handler for the
/// duration; the child starts with the default disposition, since handlers
/// do not survive `exec`. Without `sh` or a terminal the line is read plainly.
#[cfg(unix)]
fn read_hidden_line() -> Result<Option<String>> {
    use std::process::{Command, Stdio};

    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    extern "C" fn survive(_signum: i32) {}
    const SIGINT: i32 = 2;
    const SCRIPT: &str = "\
trap 'stty echo 2>/dev/null; exit 130' INT
stty -echo 2>/dev/null
IFS= read -r line
status=$?
stty echo 2>/dev/null
[ \"$status\" -eq 0 ] || exit 1
printf '%s' \"$line\"";

    if !io::stdin().is_terminal() {
        return read_plain_line();
    }
    // SAFETY: `signal(2)` has no preconditions; the handler does nothing and
    // the previous disposition is put back as soon as the child has exited.
    let previous = unsafe { signal(SIGINT, survive as usize) };
    let output = Command::new("sh")
        .args(["-c", SCRIPT])
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(Stdio::piped())
        .output();
    // SAFETY: as above.
    unsafe { signal(SIGINT, previous) };
    let output = match output {
        Ok(output) => output,
        Err(_) => return read_plain_line(),
    };
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(not(unix))]
fn read_hidden_line() -> Result<Option<String>> {
    read_plain_line()
}

fn read_plain_line() -> Result<Option<String>> {
    let mut line = String::new();
    let n = io::stdin()
        .lock()
        .read_line(&mut line)
        .context("could not read the token from standard input")?;
    Ok((n > 0).then_some(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: Option<&str>, email: Option<&str>) -> User {
        User {
            gid: "12345".to_string(),
            name: name.map(str::to_string),
            email: email.map(str::to_string),
            workspaces: Vec::new(),
        }
    }

    /// Hand-wrapped like the shared help prose: nothing wider than 78
    /// columns, bare URLs, and gh's two blank lines before USAGE.
    #[test]
    fn help_prose_is_hand_wrapped() {
        for (name, text) in [
            ("login", LOGIN_LONG_ABOUT),
            ("logout", LOGOUT_LONG_ABOUT),
            ("token", TOKEN_LONG_ABOUT),
        ] {
            assert!(text.ends_with(".\n"), "{name} should end with a newline");
            assert!(!text.ends_with("\n\n"), "{name} ends with a blank line");
            for line in text.lines() {
                assert!(line.chars().count() <= 78, "{name}: {line}");
                assert!(!line.contains("<http"), "{name}: bracketed URL: {line}");
                assert_eq!(line, line.trim_end(), "{name}: trailing space: {line:?}");
            }
        }
        for (name, text) in [
            ("login", LOGIN_EXAMPLES),
            ("logout", LOGOUT_EXAMPLES),
            ("token", TOKEN_EXAMPLES),
        ] {
            assert!(
                text.starts_with("# "),
                "{name} examples open with a comment"
            );
            for line in text.lines() {
                assert!(
                    line.is_empty() || line.starts_with("$ ") || line.starts_with("# "),
                    "{name}: not an example line: {line:?}"
                );
            }
        }
    }

    #[test]
    fn account_name_prefers_name_then_email_then_gid() {
        assert_eq!(
            account_name(&user(Some("Dave Div"), Some("d@x.y"))),
            "Dave Div"
        );
        assert_eq!(account_name(&user(Some("  "), Some("d@x.y"))), "d@x.y");
        assert_eq!(account_name(&user(None, None)), "12345");
    }

    /// gh's `auth status` sentence, naming our host, with only the command
    /// painted (bold, like `reference/gh-errors/auth-status-none.ansi.txt`).
    /// One line, no trailing period after the command, under 80 columns.
    #[test]
    fn not_logged_in_is_one_sentence_with_the_next_step() {
        let plain = not_logged_in(&Palette::off());
        assert!(plain.starts_with("You are not logged in to "), "{plain}");
        assert!(
            plain.ends_with(". To log in, run: asana auth login"),
            "{plain}"
        );
        assert!(!plain.contains('\n'), "{plain}");
        assert!(plain.chars().count() < 80, "{plain}");
        let painted = not_logged_in(&Palette::new(true));
        assert!(
            painted.ends_with("To log in, run: \x1b[0;1;39masana auth login\x1b[0m"),
            "{painted}"
        );
        assert!(
            !painted.starts_with('\x1b'),
            "only the command is painted: {painted}"
        );
    }

    /// Every failure speaks with one voice: a sentence (capital, full
    /// stop) saying what happened, then a line with the next step; where
    /// the next step is a command it follows `run:` (or the pipe hint's
    /// colon) after exactly one space. No `error:` prefix, no line wider
    /// than 79 columns, no trailing space.
    #[test]
    fn every_failure_is_a_sentence_then_a_next_step() {
        let messages = [
            ("bad token", token_rejected()),
            ("cannot prompt", cannot_prompt()),
            ("empty stdin", no_token_on_stdin().to_string()),
            ("logout with env var", logout_leaves_env(TOKEN_ENV)),
            ("logout with alt env var", logout_leaves_env(TOKEN_ENV_ALT)),
            ("not logged in", not_logged_in(&Palette::off())),
        ];
        for (name, text) in &messages {
            let lines: Vec<&str> = text.lines().collect();
            assert!(!text.is_empty(), "{name}");
            assert!(
                !text.starts_with("error"),
                "{name}: no `error:` prefix: {text}"
            );
            // A sentence opens with a capital, or with gh's `HTTP <status>:`
            // label, which `error.rs` follows with lowercase (`HTTP 401:
            // the token in ASANA_TOKEN was rejected.`).
            let first = lines[0];
            assert!(
                first.starts_with("HTTP 401: ")
                    || first.starts_with(|c: char| c.is_ascii_uppercase()),
                "{name}: opens with a capital or the HTTP label: {first}"
            );
            // The sentence closes with a full stop, either ending its line
            // or, in gh's one-line shape, before the next step.
            assert!(
                first.ends_with('.') || first.contains(". To "),
                "{name}: first line is a sentence: {first}"
            );
            for line in &lines {
                assert!(line.chars().count() <= 79, "{name}: too wide: {line}");
                assert_eq!(*line, line.trim_end(), "{name}: trailing space: {line:?}");
                assert!(
                    !line.contains(":  "),
                    "{name}: two spaces after a colon: {line}"
                );
            }
            let next = lines.last().expect("a next-step line");
            let has_next_step = next.contains("run: ")
                || next.starts_with("Pipe the token in: ")
                || next.starts_with("Check it, or create a new one at ");
            assert!(has_next_step, "{name}: names the next step: {next}");
        }
    }

    /// The pipe hint is one whole command a script can paste.
    #[test]
    fn pipe_hint_is_a_pasteable_command() {
        assert_eq!(
            PIPE_HINT,
            "Pipe the token in: echo \"$TOKEN\" | asana auth login --with-token"
        );
        assert!(cannot_prompt().ends_with(PIPE_HINT));
        assert!(no_token_on_stdin().to_string().ends_with(PIPE_HINT));
        assert_eq!(cannot_prompt().lines().count(), 2);
    }

    /// The rejected-token lines: the label, the host, that nothing was
    /// stored, and the tokens page, in two lines.
    #[test]
    fn token_rejected_names_the_host_and_the_tokens_page() {
        let text = token_rejected();
        let mut lines = text.lines();
        let first = lines.next().unwrap();
        assert!(
            first.starts_with("HTTP 401: the token was rejected by "),
            "{first}"
        );
        assert!(first.ends_with(", so it was not stored."), "{first}");
        assert_eq!(
            lines.next().unwrap(),
            format!("Check it, or create a new one at {PAT_URL}.")
        );
        assert!(lines.next().is_none());
    }

    #[test]
    fn logout_with_env_var_says_what_to_unset() {
        assert_eq!(
            logout_leaves_env("ASANA_TOKEN"),
            "The token comes from ASANA_TOKEN, which logout does not touch.\n\
             To stop using it, run: unset ASANA_TOKEN"
        );
    }
}
