//! `asana auth login`, `asana auth logout` and `asana auth token`.
//!
//! These run before a token is required, so they never fail with the
//! "populate ASANA_TOKEN" error. `login` validates the token against
//! `/users/me` and stores it in the config file; `logout` removes it;
//! `token` prints whichever token every other command would use.
//!
//! First cut wired by the orchestrator; the auth-login piece owns and
//! improves this directory. References: reference/gh/auth-login-help.txt,
//! auth-logout-help.txt, auth-token-help.txt, gh-errors/auth-login-bad-token.txt,
//! auth-logout-none.txt, auth-status-none.txt.

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{bail, Context, Result};

use crate::api::{self, ApiError, User, USER_FIELDS};
use crate::cli::{usage_error, SilentExit};
use crate::cmd::Prevalidate;
use crate::config::{self, config_path, mask_token, tilde, PAT_URL, TOKEN_ENV};
use crate::render::Palette;

const EXIT_AUTH: i32 = 4;

#[derive(clap::Args, Debug)]
#[command(
    about = "Log in to Asana with a personal access token",
    long_about = "Log in to Asana with a personal access token.\n\n\
        The token is checked against Asana and then stored in the config file\n\
        (~/.config/asana/config.toml), where every other command will find it.\n\
        Create a token at https://app.asana.com/0/my-apps.\n\n\
        Without `--with-token` the command prompts for the token; with it, the\n\
        token is read from standard input, which is what scripts should use.",
    after_help = "$ asana auth login\n$ asana auth login --with-token < token.txt\n$ echo $ASANA_TOKEN | asana auth login --with-token"
)]
pub struct LoginArgs {
    /// Read the token from standard input
    #[arg(long)]
    pub with_token: bool,
}

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
    about = "Log out of Asana",
    long_about = "Remove the stored personal access token from the config file.\n\n\
        A token set through ASANA_TOKEN or ASANA_ACCESS_TOKEN is not touched;\n\
        unset the variable to stop using it.",
    after_help = "$ asana auth logout"
)]
pub struct LogoutArgs {}
impl Prevalidate for LogoutArgs {}

#[derive(clap::Args, Debug)]
#[command(
    about = "Print the token every other command uses",
    long_about = "Print the personal access token in use, so it can be passed to other\n\
        tools: from ASANA_TOKEN, then ASANA_ACCESS_TOKEN, then the config file.",
    after_help = "$ asana auth token\n$ curl -H \"Authorization: Bearer $(asana auth token)\" https://app.asana.com/api/1.0/users/me"
)]
pub struct TokenArgs {}
impl Prevalidate for TokenArgs {}

/// `asana auth login`
pub async fn run_login(args: LoginArgs, palette: &Palette) -> Result<()> {
    let token = if args.with_token {
        read_token_from_stdin()?
    } else {
        prompt_for_token(palette)?
    };
    if token.is_empty() {
        bail!("no token was provided");
    }
    let user = validate(&token).await?;
    let path = config_path();
    write_token(&path, &token)?;
    let name = user.name.as_deref().unwrap_or("you");
    eprintln!(
        "{} Logged in to app.asana.com as {} ({})",
        palette.success_icon(),
        name,
        tilde(&path)
    );
    Ok(())
}

/// `asana auth logout`
pub async fn run_logout(_args: LogoutArgs, palette: &Palette) -> Result<()> {
    let path = config_path();
    match remove_token(&path)? {
        true => {
            eprintln!(
                "{} Logged out of app.asana.com (removed the token from {})",
                palette.success_icon(),
                tilde(&path)
            );
            Ok(())
        }
        false => {
            eprintln!("not logged in to app.asana.com");
            Err(anyhow::Error::new(SilentExit(1)))
        }
    }
}

/// `asana auth token`
pub async fn run_token(_args: TokenArgs, _palette: &Palette) -> Result<()> {
    match config::load(None) {
        Ok(cfg) => {
            println!("{}", cfg.token);
            Ok(())
        }
        Err(config::ConfigError::MissingToken(_)) => {
            eprintln!("no token found. To log in, run: asana auth login (or set {TOKEN_ENV})");
            Err(anyhow::Error::new(SilentExit(1)))
        }
        Err(err) => Err(err.into()),
    }
}

async fn validate(token: &str) -> Result<User> {
    let client = match std::env::var("ASANA_BASE_URL")
        .ok()
        .filter(|b| !b.trim().is_empty())
    {
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
            eprintln!(
                "error validating token: Asana rejected it (HTTP 401).\nCreate a new personal access token at {PAT_URL} and try again."
            );
            Err(anyhow::Error::new(SilentExit(EXIT_AUTH)))
        }
        Err(err) => Err(err.into()),
    }
}

fn read_token_from_stdin() -> Result<String> {
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .context("could not read the token from standard input")?;
    Ok(line.trim().to_string())
}

fn prompt_for_token(palette: &Palette) -> Result<String> {
    if !io::stdin().is_terminal() {
        bail!("--with-token required when not running interactively");
    }
    eprintln!(
        "{} Create a personal access token at {PAT_URL}",
        palette.warning_icon()
    );
    eprint!("? Paste your token: ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let token = line.trim().to_string();
    eprintln!("  {}", mask_token(&token));
    Ok(token)
}

/// Write `token = "..."` into the config file, keeping any other keys.
fn write_token(path: &std::path::Path, token: &str) -> Result<()> {
    let mut doc: toml::Table = match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .with_context(|| format!("{} is not valid TOML", tilde(path)))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", tilde(path))),
    };
    doc.insert("token".into(), toml::Value::String(token.to_string()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", tilde(dir)))?;
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("could not write {}", tilde(path)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Remove `token` from the config file. Returns whether one was there.
fn remove_token(path: &std::path::Path) -> Result<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", tilde(path))),
    };
    let mut doc: toml::Table = text
        .parse()
        .with_context(|| format!("{} is not valid TOML", tilde(path)))?;
    if doc.remove("token").is_none() {
        return Ok(false);
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("could not write {}", tilde(path)))?;
    Ok(true)
}
