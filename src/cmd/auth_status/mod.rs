//! `asana auth status`: who the token belongs to, where it came from, and
//! which workspace commands will use, laid out exactly like `gh auth status`:
//!
//! ```text
//! app.asana.com
//!   ✓ Logged in to app.asana.com account Dave Div (ASANA_TOKEN)
//!   - Active workspace: Personal
//!   - Email: dave@example.com
//!   - Token: 2/12************************************
//!   - Workspaces: 'Personal', 'Acme Corp'
//! ```
//!
//! A token Asana rejects prints the same block with an `X` line on stderr and
//! exits 4; a workspace that cannot be resolved is a `!` warning (exit 0).

use std::fmt::Write as _;

use anyhow::Result;

use crate::api::{ApiError, User, Workspace};
use crate::cli::SilentExit;
use crate::config::{
    mask_token, tilde, Config, TokenSource, WorkspaceSource, PAT_URL, WORKSPACE_ENV,
};
use crate::ctx::Ctx;
use crate::render::Palette;

/// The host in the header and the "Logged in to" line.
const HOST: &str = "app.asana.com";

#[derive(clap::Args, Debug)]
#[command(
    about = "Display the active account, workspace and authentication state",
    long_about = "Display the active account, workspace and authentication state.\n\n\
        The token is checked against Asana and any issues are included in the output,\n\
        together with where it was read from (ASANA_TOKEN, ASANA_ACCESS_TOKEN or\n\
        ~/.config/asana/config.toml) and the active workspace, which every command\n\
        that needs one will use.\n\n\
        To change the active workspace, pass `--workspace`, set `ASANA_WORKSPACE`, or put\n\
        `workspace = \"...\"` in ~/.config/asana/config.toml.",
    after_help = "# Check the account and workspace the CLI is using\n\
        $ asana auth status\n\
        \n\
        # Display the token instead of masking it\n\
        $ asana auth status --show-token"
)]
pub struct Args {
    /// Display the auth token
    #[arg(short = 't', long)]
    pub show_token: bool,
}

/// Nothing to check before the token is read; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for Args {}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let style = Style::new(ctx.color);
    let host = host();
    let token = if args.show_token {
        ctx.config.token.clone()
    } else {
        mask_token(&ctx.config.token)
    };
    let source = token_source(&ctx.config.token_source);

    match ctx.me().await {
        Ok(me) => {
            let resolved = ctx.workspace().await.ok();
            let active = Active::classify(&ctx.config, &me, resolved);
            let config_path = tilde(&ctx.config.config_path);
            print!(
                "{}",
                render_ok(&style, &host, &me, &active, &source, &token, &config_path)
            );
            Ok(())
        }
        Err(err) => match err.downcast_ref::<ApiError>() {
            Some(ApiError::Unauthorized { .. }) => {
                eprint!("{}", render_invalid(&style, &host, &source, &token));
                Err(SilentExit(4).into())
            }
            Some(ApiError::Network(e)) if e.is_timeout() => {
                eprint!("{}", render_timeout(&style, &host, &source));
                Err(SilentExit(1).into())
            }
            _ => Err(err),
        },
    }
}

/// `app.asana.com`, or the host of `ASANA_BASE_URL` when one is set.
fn host() -> String {
    host_of(std::env::var("ASANA_BASE_URL").ok().as_deref())
}

fn host_of(base: Option<&str>) -> String {
    base.and_then(|b| url::Url::parse(b.trim()).ok())
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| HOST.to_string())
}

/// The short label gh puts in parentheses after the account: the variable
/// name, or the config file.
fn token_source(source: &TokenSource) -> String {
    match source {
        TokenSource::Env(name) => name.to_string(),
        TokenSource::ConfigFile(path) => tilde(path),
    }
}

fn workspace_source(source: &WorkspaceSource) -> String {
    match source {
        WorkspaceSource::Flag => "--workspace".to_string(),
        WorkspaceSource::Env => WORKSPACE_ENV.to_string(),
        WorkspaceSource::ConfigFile(path) => tilde(path),
    }
}

/// `'Personal', 'Acme Corp'`, the way gh lists token scopes.
fn quoted_list<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let list: Vec<String> = names.into_iter().map(|n| format!("'{n}'")).collect();
    if list.is_empty() {
        "none".to_string()
    } else {
        list.join(", ")
    }
}

/// What `Ctx::workspace` decided, with the reason when it decided nothing.
enum Active {
    Resolved(Workspace),
    /// A preference that matched none of the user's workspaces.
    Unknown {
        value: String,
        source: String,
    },
    /// Nothing configured and more than one to choose from.
    Several(usize),
    /// The token belongs to no workspace at all.
    None,
}

impl Active {
    fn classify(config: &Config, me: &User, resolved: Option<Workspace>) -> Self {
        if let Some(ws) = resolved {
            return Active::Resolved(ws);
        }
        match &config.workspace {
            Some(pref) => Active::Unknown {
                value: pref.value.clone(),
                source: workspace_source(&pref.source),
            },
            None if me.workspaces.is_empty() => Active::None,
            None => Active::Several(me.workspaces.len()),
        }
    }
}

fn render_ok(
    s: &Style,
    host: &str,
    me: &User,
    active: &Active,
    source: &str,
    token: &str,
    config_path: &str,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", s.bold(host));
    let _ = writeln!(
        out,
        "  {} Logged in to {host} account {} ({source})",
        s.green("✓"),
        s.bold(me.display())
    );
    let pick = format!(
        "  - To pick one, pass --workspace, set {WORKSPACE_ENV}, or put `workspace = \"...\"` in {config_path}"
    );
    match active {
        Active::Resolved(ws) => {
            let _ = writeln!(out, "  - Active workspace: {}", s.bold(ws.display()));
        }
        Active::Unknown { value, source } => {
            let _ = writeln!(
                out,
                "  {} Active workspace: {} ({source}) is not one you belong to",
                s.yellow("!"),
                s.bold(&format!("\"{value}\""))
            );
            let _ = writeln!(out, "{pick}");
        }
        Active::Several(n) => {
            let _ = writeln!(
                out,
                "  {} Active workspace: {} (you belong to {n} workspaces)",
                s.yellow("!"),
                s.bold("none")
            );
            let _ = writeln!(out, "{pick}");
        }
        Active::None => {
            let _ = writeln!(
                out,
                "  {} Active workspace: {} (this token does not belong to any workspace)",
                s.yellow("!"),
                s.bold("none")
            );
        }
    }
    if let Some(email) = me.email.as_deref().filter(|e| !e.is_empty()) {
        let _ = writeln!(out, "  - Email: {}", s.bold(email));
    }
    let _ = writeln!(out, "  - Token: {}", s.bold(token));
    let _ = writeln!(
        out,
        "  - Workspaces: {}",
        s.bold(&quoted_list(me.workspaces.iter().map(Workspace::display)))
    );
    out
}

fn render_invalid(s: &Style, host: &str, source: &str, token: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", s.bold(host));
    let _ = writeln!(
        out,
        "  {} Failed to log in to {host} using token ({source})",
        s.failure_icon()
    );
    let _ = writeln!(out, "  - The token in {source} is invalid.");
    let _ = writeln!(out, "  - Token: {}", s.bold(token));
    let _ = writeln!(
        out,
        "  - To re-authenticate, replace the token in {source} with a new one from {PAT_URL}"
    );
    out
}

fn render_timeout(s: &Style, host: &str, source: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", s.bold(host));
    let _ = writeln!(
        out,
        "  {} Timeout trying to log in to {host} using token ({source})",
        s.failure_icon()
    );
    out
}

/// The exact escape sequences gh emits (`\e[0;1;39m` bold, `\e[0;32m` green,
/// ...), switched off when colour is disabled. The shared palette renders
/// `\e[1m`/`\e[32m`, which look the same but do not match the reference bytes.
#[derive(Debug, Clone, Copy)]
struct Style {
    on: bool,
    palette: Palette,
}

impl Style {
    fn new(on: bool) -> Self {
        Self {
            on,
            palette: Palette::new(on),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if !self.on || text.is_empty() {
            return text.to_string();
        }
        format!("\x1b[{code}m{text}\x1b[0m")
    }

    fn bold(&self, text: &str) -> String {
        self.paint("0;1;39", text)
    }

    fn green(&self, text: &str) -> String {
        self.paint("0;32", text)
    }

    fn yellow(&self, text: &str) -> String {
        self.paint("0;33", text)
    }

    /// The red `X` comes from the shared palette (`\e[31m`, same look) so
    /// that helper keeps a caller; this module is the only one that fails.
    fn failure_icon(&self) -> String {
        self.palette.failure_icon()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspacePref;
    use std::path::PathBuf;

    fn ws(gid: &str, name: &str) -> Workspace {
        Workspace {
            gid: gid.to_string(),
            name: Some(name.to_string()),
        }
    }

    fn me() -> User {
        User {
            gid: "1".to_string(),
            name: Some("Dave Div".to_string()),
            email: Some("dave@example.com".to_string()),
            workspaces: vec![ws("10", "Personal"), ws("20", "Acme Corp")],
        }
    }

    fn config(workspace: Option<WorkspacePref>) -> Config {
        Config {
            token: "2/1234567890/abcdef".to_string(),
            token_source: TokenSource::Env("ASANA_TOKEN"),
            workspace,
            config_path: PathBuf::from("/home/dave/.config/asana/config.toml"),
        }
    }

    #[test]
    fn happy_path_reads_like_gh() {
        let s = Style::new(false);
        let active = Active::Resolved(ws("10", "Personal"));
        let out = render_ok(
            &s,
            "app.asana.com",
            &me(),
            &active,
            "ASANA_TOKEN",
            "2/12***************",
            "~/.config/asana/config.toml",
        );
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 ✓ Logged in to app.asana.com account Dave Div (ASANA_TOKEN)\n\
             \x20 - Active workspace: Personal\n\
             \x20 - Email: dave@example.com\n\
             \x20 - Token: 2/12***************\n\
             \x20 - Workspaces: 'Personal', 'Acme Corp'\n"
        );
    }

    #[test]
    fn colour_uses_gh_sequences() {
        let s = Style::new(true);
        let active = Active::Resolved(ws("10", "Personal"));
        let out = render_ok(
            &s,
            "app.asana.com",
            &me(),
            &active,
            "ASANA_TOKEN",
            "2/12****",
            "~/.config/asana/config.toml",
        );
        assert!(out.starts_with("\x1b[0;1;39mapp.asana.com\x1b[0m\n"));
        assert!(out.contains("  \x1b[0;32m✓\x1b[0m Logged in to app.asana.com account \x1b[0;1;39mDave Div\x1b[0m (ASANA_TOKEN)\n"));
        assert!(out.contains("  - Token: \x1b[0;1;39m2/12****\x1b[0m\n"));
        assert!(Style::new(false).bold("x") == "x");
        assert!(Style::new(true).bold("").is_empty());
    }

    #[test]
    fn unresolved_workspace_is_a_warning_with_advice() {
        let s = Style::new(false);
        let user = me();
        let several = Active::classify(&config(None), &user, None);
        let out = render_ok(
            &s,
            "app.asana.com",
            &user,
            &several,
            "ASANA_TOKEN",
            "t",
            "~/c.toml",
        );
        assert!(out.contains("  ! Active workspace: none (you belong to 2 workspaces)\n"));
        assert!(out.contains(
            "  - To pick one, pass --workspace, set ASANA_WORKSPACE, or put `workspace = \"...\"` in ~/c.toml\n"
        ));

        let pref = WorkspacePref {
            value: "Nope".to_string(),
            source: WorkspaceSource::Env,
        };
        let unknown = Active::classify(&config(Some(pref)), &user, None);
        let out = render_ok(
            &s,
            "app.asana.com",
            &user,
            &unknown,
            "ASANA_TOKEN",
            "t",
            "~/c.toml",
        );
        assert!(out.contains(
            "  ! Active workspace: \"Nope\" (ASANA_WORKSPACE) is not one you belong to\n"
        ));

        let lonely = User {
            workspaces: Vec::new(),
            ..user.clone()
        };
        let none = Active::classify(&config(None), &lonely, None);
        let out = render_ok(
            &s,
            "app.asana.com",
            &lonely,
            &none,
            "ASANA_TOKEN",
            "t",
            "~/c.toml",
        );
        assert!(out.contains(
            "  ! Active workspace: none (this token does not belong to any workspace)\n"
        ));
        assert!(out.ends_with("  - Workspaces: none\n"));

        let resolved = Active::classify(&config(None), &user, Some(ws("10", "Personal")));
        assert!(matches!(resolved, Active::Resolved(w) if w.gid == "10"));
    }

    #[test]
    fn invalid_token_block() {
        let s = Style::new(false);
        let out = render_invalid(&s, "app.asana.com", "ASANA_TOKEN", "2/12****");
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 X Failed to log in to app.asana.com using token (ASANA_TOKEN)\n\
             \x20 - The token in ASANA_TOKEN is invalid.\n\
             \x20 - Token: 2/12****\n\
             \x20 - To re-authenticate, replace the token in ASANA_TOKEN with a new one from https://app.asana.com/0/my-apps\n"
        );
        let out = render_timeout(&s, "app.asana.com", "~/.config/asana/config.toml");
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 X Timeout trying to log in to app.asana.com using token (~/.config/asana/config.toml)\n"
        );
    }

    #[test]
    fn labels() {
        assert_eq!(
            token_source(&TokenSource::Env("ASANA_ACCESS_TOKEN")),
            "ASANA_ACCESS_TOKEN"
        );
        assert!(
            token_source(&TokenSource::ConfigFile(PathBuf::from("/x/config.toml")))
                .ends_with("config.toml")
        );
        assert_eq!(workspace_source(&WorkspaceSource::Flag), "--workspace");
        assert_eq!(workspace_source(&WorkspaceSource::Env), "ASANA_WORKSPACE");
        assert_eq!(quoted_list(["a", "b c"]), "'a', 'b c'");
        assert_eq!(quoted_list([]), "none");
        assert_eq!(host_of(None), "app.asana.com");
        assert_eq!(host_of(Some("")), "app.asana.com");
        assert_eq!(host_of(Some("http://127.0.0.1:8123/api/1.0")), "127.0.0.1");
    }
}
