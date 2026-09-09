//! `asana auth login`, `asana auth logout` and `asana auth token`, laid out
//! like `gh auth login`, `gh auth logout` and `gh auth token`.
//!
//! All three run before a token is required, so they never fail with the
//! "set ASANA_TOKEN" error. `login` takes a personal access token from a
//! prompt or, with `--with-token`, from standard input, checks it against
//! `/users/me`, and stores it in the config file (mode 600, other keys and
//! comments kept); `logout` removes it; `token` prints whichever token every
//! other command would use.
//!
//! What each prints, and the references it is judged against:
//!
//! ```text
//! $ echo $TOKEN | asana auth login --with-token
//! ✓ Logged in to app.asana.com account Dave Div (~/.config/asana/config.toml)
//!
//! $ echo notarealtoken | asana auth login --with-token      (gh-errors/auth-login-bad-token)
//! error validating token: HTTP 401: Not Authorized (https://app.asana.com/api/1.0/users/me)
//! Check the token or create a new one at https://app.asana.com/0/my-apps, then try again.
//! [exit 4]
//!
//! $ asana auth login < /dev/null                            (gh-errors/create-noninteractive)
//! --with-token required when not running interactively
//!
//! Usage:  asana auth login [flags]
//! ...
//! [exit 1]
//!
//! $ asana auth logout                                       (gh-errors/auth-logout-none)
//! not logged in to app.asana.com
//! [exit 1]
//!
//! $ asana auth token                                        (gh-errors/auth-status-none)
//! You are not logged in to app.asana.com. To log in, run: asana auth login
//! [exit 1]
//! ```
//!
//! The account name is kept beside the token (`user = "..."`) so `logout`
//! can say whose token it removed, the way `gh auth logout` names the
//! account, without a network call.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::api::{self, ApiError, User, USER_FIELDS};
use crate::cli::{usage_error, Cancelled, SilentExit};
use crate::cmd::{confirm, Prevalidate};
use crate::config::{config_path, load, tilde, ConfigError, PAT_URL, TOKEN_ENV, TOKEN_ENV_ALT};
use crate::render::Palette;

const EXIT_ERROR: i32 = 1;
const EXIT_AUTH: i32 = 4;

/// The host named in every line, as `gh` names `github.com`.
const HOST: &str = "app.asana.com";

/// The config keys `login` writes and `logout` removes. `config::load` reads
/// `token`; `user` is the account name, for `logout`'s message and the
/// "already logged in" question.
const TOKEN_KEY: &str = "token";
const USER_KEY: &str = "user";

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

/// gh's own check, word for word: a prompt needs a terminal on every
/// stream, so a script must pass `--with-token`. A usage error, exit 1,
/// before the token is read.
impl Prevalidate for LoginArgs {
    fn prevalidate(&self, interactive: bool) -> Result<()> {
        if !self.with_token && !interactive {
            return Err(usage_error(
                &["auth", "login"],
                "--with-token required when not running interactively",
            ));
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
    let path = config_path();
    let token = if args.with_token {
        read_token_from_stdin()?
    } else {
        match prompt_for_token(&path)? {
            Some(token) => token,
            // Declined to replace the stored token: nothing to do, like gh.
            None => return Ok(()),
        }
    };
    let user = validate(&token).await?;
    let name = account_name(&user);
    write_token(&path, &token, Some(name.as_str()))?;
    eprintln!(
        "{} Logged in to {} account {} ({})",
        palette.success_icon(),
        host(),
        palette.bold(&name),
        tilde(&path)
    );
    if let Some(var) = env_token_var() {
        eprintln!(
            "{} The {var} environment variable is set and takes precedence over the stored token",
            palette.warning_icon()
        );
    }
    Ok(())
}

/// `asana auth logout`: remove the stored token, naming the account it
/// belonged to; one line and exit 1 when there is none, like gh's `not
/// logged in to any hosts`.
pub async fn run_logout(_args: LogoutArgs, palette: &Palette) -> Result<()> {
    let path = config_path();
    let host = host();
    match remove_token(&path)? {
        Some(removed) => {
            match removed.user {
                Some(user) => eprintln!(
                    "{} Logged out of {host} account {}",
                    palette.success_icon(),
                    palette.bold(&user)
                ),
                None => eprintln!("{} Logged out of {host}", palette.success_icon()),
            }
            if let Some(var) = env_token_var() {
                eprintln!(
                    "{} The {var} environment variable is still set and will be used for authentication",
                    palette.warning_icon()
                );
            }
            Ok(())
        }
        None => {
            eprintln!("not logged in to {host}");
            if let Some(var) = env_token_var() {
                eprintln!(
                    "The {var} environment variable is being used for authentication; unset it to stop using it."
                );
            }
            Err(SilentExit(EXIT_ERROR).into())
        }
    }
}

/// `asana auth token`: the token every other command uses, alone on stdout
/// so it can be substituted into another command.
pub async fn run_token(_args: TokenArgs, palette: &Palette) -> Result<()> {
    match load(None) {
        Ok(cfg) => {
            println!("{}", cfg.token);
            Ok(())
        }
        Err(ConfigError::MissingToken(_)) => {
            eprintln!(
                "You are not logged in to {}. To log in, run: {}",
                host(),
                palette.bold("asana auth login")
            );
            Err(SilentExit(EXIT_ERROR).into())
        }
        Err(err) => Err(err.into()),
    }
}

// ------------------------------------------------------------- validation

/// `GET /users/me` with the candidate token. A 401 is gh's `error
/// validating token: <transport line>` and a next step, exit 4; anything
/// else (network, 5xx) is reported the way every other command reports it.
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
        Err(err @ ApiError::Unauthorized { .. }) => {
            eprintln!("error validating token: {err}");
            eprintln!("Check the token or create a new one at {PAT_URL}, then try again.");
            Err(SilentExit(EXIT_AUTH).into())
        }
        Err(err) => Err(err.into()),
    }
}

/// `ASANA_BASE_URL` when set (a mock server), else the production API.
fn base_url() -> Option<String> {
    std::env::var("ASANA_BASE_URL")
        .ok()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
}

/// `app.asana.com`, or the host of `ASANA_BASE_URL` when one is set, so the
/// messages name the server that was actually asked.
fn host() -> String {
    base_url()
        .and_then(|b| url::Url::parse(&b).ok())
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| HOST.to_string())
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
        bail!("no token was read from standard input");
    }
    Ok(token)
}

/// The interactive path, shaped like gh's "Paste an authentication token"
/// flow: an "already logged in" question when a token is stored, the tip
/// with the token page, then the hidden prompt. `None` when the user keeps
/// the stored token.
fn prompt_for_token(path: &Path) -> Result<Option<String>> {
    if let Some(stored) = read_stored(path)? {
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

// ------------------------------------------------------------ config file

/// What `login` left in the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stored {
    /// The account name written beside the token, when there was one.
    user: Option<String>,
}

/// The stored login, if the file holds a non-empty `token`.
fn read_stored(path: &Path) -> Result<Option<Stored>> {
    let Some(text) = read_config(path)? else {
        return Ok(None);
    };
    let doc = parse(path, &text)?;
    let has_token = doc
        .get(TOKEN_KEY)
        .and_then(toml::Value::as_str)
        .is_some_and(|t| !t.trim().is_empty());
    Ok(has_token.then(|| Stored {
        user: user_of(&doc),
    }))
}

fn user_of(doc: &toml::Table) -> Option<String> {
    doc.get(USER_KEY)
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
}

/// The file's text, or `None` when there is no file yet.
fn read_config(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("could not read {}", tilde(path))),
    }
}

fn parse(path: &Path, text: &str) -> Result<toml::Table> {
    text.parse()
        .with_context(|| format!("could not parse {}", tilde(path)))
}

/// Store `token` (and the account name) in the config file, creating it
/// readable only by the owner, and keeping every other key, comment and
/// blank line where it was.
fn write_token(path: &Path, token: &str, user: Option<&str>) -> Result<()> {
    let text = read_config(path)?.unwrap_or_default();
    let mut doc = parse(path, &text)?;
    let edited = edit_top_level(&text, &[(TOKEN_KEY, Some(token)), (USER_KEY, user)]);
    // A file the line editor cannot handle (a key spread over several
    // lines) is rewritten from the parsed document instead, which keeps the
    // keys but not the comments.
    let text = match edited.parse::<toml::Table>() {
        Ok(_) => edited,
        Err(_) => {
            doc.insert(TOKEN_KEY.into(), toml::Value::String(token.to_string()));
            match user {
                Some(u) => doc.insert(USER_KEY.into(), toml::Value::String(u.to_string())),
                None => doc.remove(USER_KEY),
            };
            doc.to_string()
        }
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", tilde(dir)))?;
    }
    write_private(path, &text).with_context(|| format!("could not write {}", tilde(path)))?;
    Ok(())
}

/// Remove `token` and `user` from the config file. What was there, or
/// `None` when no token was stored (the file is left alone then). A file
/// with nothing else in it, which `login` created, is removed.
fn remove_token(path: &Path) -> Result<Option<Stored>> {
    let Some(text) = read_config(path)? else {
        return Ok(None);
    };
    let mut doc = parse(path, &text)?;
    let had_token = doc
        .get(TOKEN_KEY)
        .and_then(toml::Value::as_str)
        .is_some_and(|t| !t.trim().is_empty());
    if !had_token {
        return Ok(None);
    }
    let stored = Stored {
        user: user_of(&doc),
    };
    doc.remove(TOKEN_KEY);
    doc.remove(USER_KEY);
    let edited = edit_top_level(&text, &[(TOKEN_KEY, None), (USER_KEY, None)]);
    let text = match edited.parse::<toml::Table>() {
        Ok(_) => edited,
        Err(_) => doc.to_string(),
    };
    if text.trim().is_empty() {
        std::fs::remove_file(path).with_context(|| format!("could not remove {}", tilde(path)))?;
    } else {
        write_private(path, &text).with_context(|| format!("could not write {}", tilde(path)))?;
    }
    Ok(Some(stored))
}

/// Write the file so that only the owner can read it: created with mode
/// 600, and set to 600 when it already existed with something wider.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Set (or, with `None`, remove) top-level keys in a TOML document by
/// editing its lines, so comments, blank lines and every other key stay as
/// the user wrote them. A key's line is replaced in place (duplicates are
/// dropped); a new key goes after the last top-level line, before the first
/// `[table]` header. The result is not guaranteed to parse for exotic files;
/// callers check it and fall back to a full rewrite.
fn edit_top_level(text: &str, changes: &[(&str, Option<&str>)]) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    for (key, value) in changes {
        let mut in_root = true;
        let mut end_of_root = 0;
        let mut found = None;
        let mut kept = Vec::with_capacity(lines.len());
        for line in lines.drain(..) {
            let trimmed = line.trim_start();
            if in_root && trimmed.starts_with('[') {
                in_root = false;
            }
            if in_root && is_key_line(trimmed, key) {
                if found.is_none() {
                    found = Some(kept.len());
                    kept.push(String::new()); // placeholder, filled below
                    end_of_root = kept.len();
                }
                continue;
            }
            if in_root && !trimmed.is_empty() {
                end_of_root = kept.len() + 1;
            }
            kept.push(line);
        }
        lines = kept;
        let assignment = value.map(|v| format!("{key} = {}", toml::Value::String(v.to_string())));
        match (found, assignment) {
            (Some(i), Some(line)) => lines[i] = line,
            (Some(i), None) => {
                lines.remove(i);
            }
            (None, Some(line)) => lines.insert(end_of_root.min(lines.len()), line),
            (None, None) => {}
        }
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// `key = ...` (or `key=...`), and not `key_other = ...`.
fn is_key_line(trimmed: &str, key: &str) -> bool {
    trimmed
        .strip_prefix(key)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
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

    #[test]
    fn key_lines_match_the_key_only() {
        assert!(is_key_line("token = \"x\"", "token"));
        assert!(is_key_line("token=\"x\"", "token"));
        assert!(!is_key_line("token_alt = \"x\"", "token"));
        assert!(!is_key_line("# token = \"x\"", "token"));
        assert!(!is_key_line("mytoken = 1", "token"));
    }

    #[test]
    fn editing_keeps_comments_and_other_keys() {
        let text = "# asana config\nworkspace = \"Personal\"\n\n[other]\ntoken = \"not ours\"\n";
        let out = edit_top_level(text, &[("token", Some("2/1/abc")), ("user", Some("Dave"))]);
        assert_eq!(
            out,
            "# asana config\nworkspace = \"Personal\"\ntoken = \"2/1/abc\"\nuser = \"Dave\"\n\n[other]\ntoken = \"not ours\"\n"
        );
        // Replace in place, dropping a duplicate.
        let text = "token = \"old\"\nworkspace = \"P\"\ntoken=\"older\"\n";
        let out = edit_top_level(text, &[("token", Some("new"))]);
        assert_eq!(out, "token = \"new\"\nworkspace = \"P\"\n");
        // Remove, leaving the rest untouched.
        let out = edit_top_level(&out, &[("token", None), ("user", None)]);
        assert_eq!(out, "workspace = \"P\"\n");
        // From nothing.
        assert_eq!(
            edit_top_level("", &[("token", Some("t"))]),
            "token = \"t\"\n"
        );
        assert_eq!(edit_top_level("", &[("token", None)]), "");
        // Quotes and backslashes come out as TOML needs them (toml picks a
        // literal string when that is simpler).
        let out = edit_top_level("", &[("user", Some("Dave \"D\" \\ Div"))]);
        assert!(out.starts_with("user = ") && out.ends_with('\n'), "{out}");
        assert_eq!(
            out.parse::<toml::Table>().unwrap()["user"].as_str(),
            Some("Dave \"D\" \\ Div")
        );
    }

    #[test]
    fn write_creates_a_private_file_and_remove_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asana").join("config.toml");
        write_token(&path, "2/1/secret", Some("Dave Div")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "token = \"2/1/secret\"\nuser = \"Dave Div\"\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode {mode:o}");
        }
        assert_eq!(
            read_stored(&path).unwrap(),
            Some(Stored {
                user: Some("Dave Div".to_string())
            })
        );
        assert_eq!(
            remove_token(&path).unwrap(),
            Some(Stored {
                user: Some("Dave Div".to_string())
            })
        );
        assert!(!path.exists(), "a file login created is removed with it");
        assert_eq!(remove_token(&path).unwrap(), None);
        assert_eq!(read_stored(&path).unwrap(), None);
    }

    #[test]
    fn write_merges_with_existing_keys_and_fixes_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "# mine\nworkspace = \"Personal\"\ntoken = \"old\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_token(&path, "new", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\nworkspace = \"Personal\"\ntoken = \"new\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode {mode:o}");
        }
        // The stored login has no name; logout still succeeds.
        assert_eq!(read_stored(&path).unwrap(), Some(Stored { user: None }));
        assert_eq!(remove_token(&path).unwrap(), Some(Stored { user: None }));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\nworkspace = \"Personal\"\n"
        );
    }

    #[test]
    fn remove_leaves_a_missing_or_tokenless_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(remove_token(&path).unwrap(), None);
        assert!(!path.exists());
        std::fs::write(&path, "workspace = \"P\"\n").unwrap();
        assert_eq!(remove_token(&path).unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "workspace = \"P\"\n"
        );
    }

    #[test]
    fn malformed_config_is_an_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = \n").unwrap();
        let err = write_token(&path, "t", None).unwrap_err();
        assert!(err.to_string().starts_with("could not parse "), "{err}");
        let err = remove_token(&path).unwrap_err();
        assert!(err.to_string().starts_with("could not parse "), "{err}");
    }

    /// A file the line editor cannot follow (a multi-line value) is rewritten
    /// from the parsed document, so the keys survive even if comments do not.
    #[test]
    fn exotic_files_fall_back_to_a_full_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = \"\"\"\nold\n\"\"\"\nworkspace = \"P\"\n").unwrap();
        write_token(&path, "new", Some("Dave")).unwrap();
        let doc: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["token"].as_str(), Some("new"));
        assert_eq!(doc["user"].as_str(), Some("Dave"));
        assert_eq!(doc["workspace"].as_str(), Some("P"));
    }
}
