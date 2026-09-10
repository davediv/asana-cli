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
//! The token was rejected by app.asana.com (HTTP 401), so it was not stored.
//! Check it, or create a new one at https://app.asana.com/0/my-apps.
//! [exit 4]
//!
//! $ asana auth login < /dev/null                            (gh-errors/create-noninteractive)
//! A token cannot be prompted for when not running interactively.
//! To pipe one in, run:  echo "$TOKEN" | asana auth login --with-token
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
//! [exit 4]
//!
//! $ asana auth token                                        (gh-errors/auth-status-none)
//! You are not logged in to app.asana.com. To log in, run: asana auth login
//! [exit 4]
//! ```
//!
//! # One voice for every failure
//!
//! gh speaks three ways here: `error validating token: HTTP 401: ...` for
//! a bad token, the bare `not logged in to any hosts` for a logout with
//! nothing stored, and the sentence `You are not logged into any GitHub
//! hosts. To log in, run: gh auth login` for status. Every failure in this
//! file has one shape instead, the shape of that last line: a sentence
//! that leads with its subject and says what happened (`You are not logged
//! in ...`, `The token was rejected ...`, `A token cannot be prompted for
//! ...`), then the next step, which names the command to run after `run:`
//! where there is one. No `error:` prefix and no `HTTP 401:` label opening
//! the line (the status sits in parentheses inside the sentence), no
//! lowercase fragment, nothing wider than 79 columns. A command after a
//! colon follows two spaces, as gh's `Try authenticating with:  gh auth
//! login` and every next step `error.rs` prints do; the one exception is
//! the shared not-logged-in sentence, which keeps gh's own one-space
//! spelling. A usage error (the prompt without a terminal) keeps the
//! `Usage:` block under its two lines, as every usage error in the CLI
//! does.
//!
//! # Exit codes
//!
//! `asana help exit-codes` gives 4 to authentication required or rejected
//! and 1 to every other failure. Every auth-state failure here is in the
//! one family, 4, so a script can branch on it: a token Asana rejects
//! (the code `task list` exits with when its token is rejected), and the
//! not-logged-in state met by `logout` and `token`, the same code `auth
//! status` exits with for the same sentence. A logout that finds nothing
//! stored is in that family too, whether or not an environment variable is
//! doing the authenticating. The prompt without a terminal and an empty
//! standard input are usage errors, 1: a script that wants a no-op logout
//! writes `asana auth logout 2>/dev/null || true`, the way it would for
//! `rm`. gh exits 1 for its `not logged in to any hosts`; ours follows the
//! design contract's table instead, which puts every auth state under 4.
//!
//! The account name is kept beside the token (`user = "..."`) so `logout`
//! can say whose token it removed, the way `gh auth logout` names the
//! account, without a network call.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{anyhow, Context, Result};

use crate::api::{self, ApiError, User, USER_FIELDS};
use crate::cli::{usage_error, Cancelled, SilentExit};
use crate::cmd::{confirm, not_logged_in, Prevalidate};
use crate::config::{
    base_url, host, load, remove_token, save_login, stored_login, tilde, ConfigError, PAT_URL,
    TOKEN_ENV, TOKEN_ENV_ALT,
};
use crate::render::Palette;

/// Authentication required or rejected (`asana help exit-codes`): the one
/// code for every auth-state failure in this file.
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
/// and exit 4, the not-logged-in code.
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
/// not-logged-in line as `logout` and `auth status`, exit 4.
pub async fn run_token(_args: TokenArgs, palette: &Palette) -> Result<()> {
    match load(None) {
        Ok(cfg) => {
            println!("{}", cfg.token);
            Ok(())
        }
        Err(ConfigError::MissingToken(_)) => {
            eprintln!("{}", not_logged_in(palette));
            Err(SilentExit(EXIT_AUTH).into())
        }
        Err(err) => Err(err.into()),
    }
}

// --------------------------------------------------------------- messages
//
// Every failure below is one sentence that leads with its subject and says
// what happened, then the next step; a command to run follows `run:` and
// two spaces (`error.rs`'s spelling, from gh's `Try authenticating with:
// gh auth login`). Each line is kept inside its 79 columns by hand. The
// usage errors (exit 1) are returned as errors for `error::report` to
// print; the auth-state failures (exit 4) are printed here, on stderr, and
// finished with `SilentExit`, since `report` has no code for them.

/// The next step for a script: the token on standard input.
const PIPE_HINT: &str = "To pipe one in, run:  echo \"$TOKEN\" | asana auth login --with-token";

/// `logout` found no stored token. Nothing was removed, so this is a
/// failure, and the not-logged-in kind (exit 4, where gh prints `not
/// logged in to any hosts`): the shared not-logged-in sentence, or, when
/// an environment variable is what every command authenticates with, that
/// there is nothing stored and the variable to unset.
fn nothing_to_log_out(palette: &Palette) -> anyhow::Error {
    let text = match env_token_var() {
        Some(var) => logout_leaves_env(var),
        None => not_logged_in(palette),
    };
    eprintln!("{text}");
    SilentExit(EXIT_AUTH).into()
}

/// The login is `var`, which `logout` cannot remove: that nothing is
/// stored, where the token comes from, and how to stop using it.
fn logout_leaves_env(var: &str) -> String {
    format!(
        "No token is stored to remove; the one in use comes from {var}.\n\
         To stop using it, run:  unset {var}"
    )
}

/// `login` without `--with-token` and without a terminal to prompt on:
/// what cannot happen and why, then the way in for a script.
fn cannot_prompt() -> String {
    format!("A token cannot be prompted for when not running interactively.\n{PIPE_HINT}")
}

/// `--with-token` read an empty line: the same next step.
fn no_token_on_stdin() -> anyhow::Error {
    anyhow!("No token was read from standard input.\n{PIPE_HINT}")
}

/// Asana answered 401 to the candidate token: what happened, with the
/// status in parentheses rather than opening the line as `error.rs`'s
/// `HTTP 401:` label does, so the sentence has the shape of every other
/// failure here; that the token went nowhere; and the next step, which is
/// the token itself (`asana auth login` is what the user just ran, so it
/// is not repeated). gh's `error validating token:` prefix and the
/// endpoint that saw the 401 are left out: a rejected token is rejected
/// everywhere.
fn token_rejected() -> String {
    format!(
        "The token was rejected by {} (HTTP 401), so it was not stored.\n\
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

    /// The shared not-logged-in sentence (`cmd::not_logged_in`) is the one
    /// `logout` and `token` print, so it cannot drift from `auth status`:
    /// gh's `auth status` line, naming our host, with only the command
    /// painted (bold, like `reference/gh-errors/auth-status-none.ansi.txt`).
    /// One line, no trailing period after the command, under 80 columns.
    #[test]
    fn not_logged_in_is_one_sentence_with_the_next_step() {
        let plain = not_logged_in(&Palette::off());
        assert_eq!(
            plain,
            format!(
                "You are not logged in to {}. To log in, run: asana auth login",
                host()
            )
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

    /// Every failure speaks with one voice: a sentence that leads with its
    /// subject (a capital letter, never an `error:` prefix or an `HTTP`
    /// label) and says what happened, then the next step; where the next
    /// step is a command it follows `run:` and two spaces, gh's spelling
    /// in `Try authenticating with:  gh auth login` and `error.rs`'s in
    /// every hint it prints. The one exception is the shared not-logged-in
    /// sentence, which keeps gh's own one-space `To log in, run: `. No
    /// line wider than 79 columns, no trailing space.
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
            let first = lines[0];
            assert!(
                first.starts_with(|c: char| c.is_ascii_uppercase()),
                "{name}: leads with its subject: {first}"
            );
            assert!(
                !first.starts_with("HTTP "),
                "{name}: no label opens the line: {first}"
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
            }
            let next = lines.last().expect("a next-step line");
            let has_next_step = next.contains("run:  ")
                || next.contains("To log in, run: asana auth login")
                || next.starts_with("Check it, or create a new one at ");
            assert!(has_next_step, "{name}: names the next step: {next}");
            // A command after `run:` follows two spaces (one in the shared
            // gh sentence), never one, never three.
            if let Some((_, command)) = next.split_once("run:") {
                let spaces = command.len() - command.trim_start().len();
                let expected = if *name == "not logged in" { 1 } else { 2 };
                assert_eq!(spaces, expected, "{name}: spaces after `run:`: {next:?}");
            }
        }
    }

    /// The pipe hint is one whole command a script can paste, after the
    /// two spaces that set a command off from its sentence.
    #[test]
    fn pipe_hint_is_a_pasteable_command() {
        assert_eq!(
            PIPE_HINT,
            "To pipe one in, run:  echo \"$TOKEN\" | asana auth login --with-token"
        );
        assert!(cannot_prompt().ends_with(PIPE_HINT));
        assert!(no_token_on_stdin().to_string().ends_with(PIPE_HINT));
        assert_eq!(cannot_prompt().lines().count(), 2);
        assert_eq!(
            cannot_prompt().lines().next().unwrap(),
            "A token cannot be prompted for when not running interactively."
        );
    }

    /// The rejected-token lines: the host, the status in parentheses, that
    /// nothing was stored, and the tokens page, in two lines.
    #[test]
    fn token_rejected_names_the_host_and_the_tokens_page() {
        let text = token_rejected();
        let mut lines = text.lines();
        let first = lines.next().unwrap();
        assert_eq!(
            first,
            format!(
                "The token was rejected by {} (HTTP 401), so it was not stored.",
                host()
            )
        );
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
            "No token is stored to remove; the one in use comes from ASANA_TOKEN.\n\
             To stop using it, run:  unset ASANA_TOKEN"
        );
    }

    /// Every auth-state failure exits with the one code, 4 (`asana help
    /// exit-codes`: authentication required or rejected), the code `auth
    /// status` uses for the same sentence; the usage errors exit 1 through
    /// `error::report`.
    #[test]
    fn auth_state_failures_share_the_auth_exit_code() {
        assert_eq!(EXIT_AUTH, crate::error::EXIT_AUTH);
        let err = nothing_to_log_out(&Palette::off());
        let silent = err
            .downcast_ref::<SilentExit>()
            .expect("printed here, finished with SilentExit");
        assert_eq!(silent.0, EXIT_AUTH);
        assert!(no_token_on_stdin().downcast_ref::<SilentExit>().is_none());
        let usage = usage_error(&["auth", "login"], cannot_prompt());
        assert!(usage.downcast_ref::<SilentExit>().is_none());
    }
}
