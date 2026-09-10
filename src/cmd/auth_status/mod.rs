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
//! The states gh reports, and what each does here:
//!
//! - no token anywhere: gh's one line (`You are not logged into any GitHub
//!   hosts. To log in, run: gh auth login`), on stderr, exit 4, the same
//!   sentence `auth logout` and `auth token` print. It is decided in
//!   `Prevalidate`, before `main` reads the config, so the state is this
//!   command's answer rather than the generic "set ASANA_TOKEN" error every
//!   other command gets;
//! - a token Asana rejects: the same block with an `X` line, on stderr,
//!   exit 4;
//! - a workspace that cannot be resolved: a `!` line with how to pick one,
//!   exit 0 (the token works; only the preference is off).

use std::fmt::Write as _;
use std::io::IsTerminal;

use anyhow::Result;

use crate::api::{ApiError, User, Workspace};
use crate::cli::SilentExit;
use crate::cmd::Prevalidate;
use crate::config::{
    self, mask_token, tilde, Config, ConfigError, TokenSource, PAT_URL, WORKSPACE_ENV,
};
use crate::ctx::Ctx;
use crate::render::Palette;

const EXIT_ERROR: i32 = 1;
const EXIT_AUTH: i32 = 4;

/// gh's description, said of one host and one token; the `about` alone is
/// what `asana auth --help` lists, short enough to sit after `status:` on
/// an 80-column screen. Hand-wrapped short of 78 columns like the rest of
/// the help prose; the trailing newline leaves gh's two blank lines before
/// USAGE.
const LONG_ABOUT: &str = "\
Display the active account and authentication state.

The token is checked against Asana and any issues are included in the
output, together with where it was read from (ASANA_TOKEN, ASANA_ACCESS_TOKEN
or ~/.config/asana/config.toml) and the active workspace, which every command
that needs one will use.

If there is no token, or Asana rejects it, the command exits with 4 and
reports on stderr.

To change the active workspace, pass `--workspace`, set `ASANA_WORKSPACE`, or
put `workspace = \"...\"` in ~/.config/asana/config.toml.

To change the account, see `asana auth login`.
";

const EXAMPLES: &str = "\
# Display authentication status for the account and workspace in use
$ asana auth status

# Display authentication status with another workspace active
$ asana auth status --workspace \"Acme Corp\"

# Display the token in plain text
$ asana auth status --show-token";

#[derive(clap::Args, Debug)]
#[command(
    about = "Display the active account and authentication state",
    long_about = LONG_ABOUT,
    after_help = EXAMPLES
)]
pub struct Args {
    /// Display the auth token
    #[arg(short = 't', long)]
    pub show_token: bool,
}

impl Prevalidate for Args {
    /// No token anywhere is this command's answer, not a failure to set it
    /// up: gh's one not-logged-in line, exit 4. Anything else (a token, an
    /// unreadable or malformed config file) is left to `config::load`,
    /// which reports it the way every command does.
    fn prevalidate(&self, _interactive: bool) -> Result<()> {
        match config::load(None) {
            Err(ConfigError::MissingToken(_)) => {
                eprintln!("{}", not_logged_in(&stderr_palette()));
                Err(SilentExit(EXIT_AUTH).into())
            }
            _ => Ok(()),
        }
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let palette = Palette::new(ctx.color);
    let host = config::host();
    let token = if args.show_token {
        ctx.config.token.clone()
    } else {
        mask_token(&ctx.config.token)
    };
    let source = &ctx.config.token_source;

    match ctx.me().await {
        Ok(me) => {
            let resolved = ctx.workspace().await.ok();
            let active = Active::classify(&ctx.config, &me, resolved);
            let config_path = tilde(&ctx.config.config_path);
            print!(
                "{}",
                render_ok(
                    &palette,
                    &host,
                    &me,
                    &active,
                    &source.label(),
                    &token,
                    &config_path
                )
            );
            Ok(())
        }
        Err(err) => match err.downcast_ref::<ApiError>() {
            Some(ApiError::Unauthorized { .. }) => {
                eprint!(
                    "{}",
                    render_invalid(&stderr_palette(), &host, source, &token)
                );
                Err(SilentExit(EXIT_AUTH).into())
            }
            Some(ApiError::Network(e)) if e.is_timeout() => {
                eprint!(
                    "{}",
                    render_timeout(&stderr_palette(), &host, &source.label())
                );
                Err(SilentExit(EXIT_ERROR).into())
            }
            _ => Err(err),
        },
    }
}

/// The palette for what this command writes on stderr, from stderr's own
/// terminal-ness (`main` builds stdout's the same way): `asana auth status
/// 2>log` keeps the log clean, and `--no-color`, `NO_COLOR` and `TERM=dumb`
/// switch it off as usual. The flag is read from the command line because
/// `Prevalidate` runs before `Ctx` exists.
fn stderr_palette() -> Palette {
    let no_color_flag = std::env::args_os().any(|a| a == "--no-color");
    Palette::for_stream(std::io::stderr().is_terminal(), no_color_flag)
}

/// gh's `You are not logged into any GitHub hosts. To log in, run: gh auth
/// login`, said of the host we would have asked, with the command in bold
/// as gh paints it. The same sentence `auth logout` and `auth token` print.
fn not_logged_in(palette: &Palette) -> String {
    format!(
        "You are not logged in to {}. To log in, run: {}",
        config::host(),
        palette.bold("asana auth login")
    )
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
                source: pref.source.label(),
            },
            None if me.workspaces.is_empty() => Active::None,
            None => Active::Several(me.workspaces.len()),
        }
    }
}

fn render_ok(
    p: &Palette,
    host: &str,
    me: &User,
    active: &Active,
    source: &str,
    token: &str,
    config_path: &str,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", p.bold(host));
    let _ = writeln!(
        out,
        "  {} Logged in to {host} account {} ({source})",
        p.success_icon(),
        p.bold(me.display())
    );
    // Hand-wrapped, the continuation hanging under the bullet's text, so
    // the config path never pushes the line past an 80-column screen.
    let pick = format!(
        "  - To pick one, pass --workspace, set {WORKSPACE_ENV}, or put\n    `workspace = \"...\"` in {config_path}"
    );
    match active {
        Active::Resolved(ws) => {
            let _ = writeln!(out, "  - Active workspace: {}", p.bold(ws.display()));
        }
        Active::Unknown { value, source } => {
            let _ = writeln!(
                out,
                "  {} Active workspace: {} ({source}) is not one you belong to",
                p.warning_icon(),
                p.bold(&format!("\"{value}\""))
            );
            let _ = writeln!(out, "{pick}");
        }
        Active::Several(n) => {
            let _ = writeln!(
                out,
                "  {} Active workspace: {} (you belong to {n} workspaces)",
                p.warning_icon(),
                p.bold("none")
            );
            let _ = writeln!(out, "{pick}");
        }
        Active::None => {
            let _ = writeln!(
                out,
                "  {} Active workspace: {} (this token does not belong to any workspace)",
                p.warning_icon(),
                p.bold("none")
            );
        }
    }
    if let Some(email) = me.email.as_deref().filter(|e| !e.is_empty()) {
        let _ = writeln!(out, "  - Email: {}", p.bold(email));
    }
    let _ = writeln!(out, "  - Token: {}", p.bold(token));
    let _ = writeln!(
        out,
        "  - Workspaces: {}",
        p.bold(&quoted_list(me.workspaces.iter().map(Workspace::display)))
    );
    out
}

/// gh's block for a rejected token (`X Failed to log in to github.com
/// account davediv (keyring)`, `The token in keyring is invalid.`, `To
/// re-authenticate, run: gh auth login`), with the way back that fits where
/// the token came from: `asana auth login` rewrites the config file, while
/// a variable has to be set again (or unset, so that login can take over).
fn render_invalid(p: &Palette, host: &str, source: &TokenSource, token: &str) -> String {
    let label = source.label();
    let mut out = String::new();
    let _ = writeln!(out, "{}", p.bold(host));
    let _ = writeln!(
        out,
        "  {} Failed to log in to {host} using token ({label})",
        p.failure_icon()
    );
    let _ = writeln!(out, "  - The token in {label} is invalid.");
    let _ = writeln!(out, "  - Token: {}", p.bold(token));
    match source {
        TokenSource::ConfigFile(_) => {
            let _ = writeln!(
                out,
                "  - To re-authenticate, run: {}",
                p.bold("asana auth login")
            );
        }
        TokenSource::Env(_) => {
            let _ = writeln!(
                out,
                "  - To re-authenticate, set {label} to a new token from {PAT_URL}"
            );
            let _ = writeln!(
                out,
                "  - To log in another way, unset {label} and run: {}",
                p.bold("asana auth login")
            );
        }
    }
    out
}

fn render_timeout(p: &Palette, host: &str, source: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", p.bold(host));
    let _ = writeln!(
        out,
        "  {} Timeout trying to log in to {host} using token ({source})",
        p.failure_icon()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{WorkspacePref, WorkspaceSource};
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
        let p = Palette::off();
        let active = Active::Resolved(ws("10", "Personal"));
        let out = render_ok(
            &p,
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

    /// The bytes are gh's (reference/gh/auth-status.ansi.txt): the host,
    /// the account, and each value in `\e[0;1;39m`, the tick in `\e[0;32m`,
    /// and the labels plain.
    #[test]
    fn colour_uses_gh_sequences() {
        let p = Palette::new(true);
        let active = Active::Resolved(ws("10", "Personal"));
        let out = render_ok(
            &p,
            "app.asana.com",
            &me(),
            &active,
            "ASANA_TOKEN",
            "2/12****",
            "~/.config/asana/config.toml",
        );
        assert_eq!(
            out,
            "\x1b[0;1;39mapp.asana.com\x1b[0m\n\
             \x20 \x1b[0;32m✓\x1b[0m Logged in to app.asana.com account \x1b[0;1;39mDave Div\x1b[0m (ASANA_TOKEN)\n\
             \x20 - Active workspace: \x1b[0;1;39mPersonal\x1b[0m\n\
             \x20 - Email: \x1b[0;1;39mdave@example.com\x1b[0m\n\
             \x20 - Token: \x1b[0;1;39m2/12****\x1b[0m\n\
             \x20 - Workspaces: \x1b[0;1;39m'Personal', 'Acme Corp'\x1b[0m\n"
        );
    }

    #[test]
    fn unresolved_workspace_is_a_warning_with_advice() {
        let p = Palette::off();
        let user = me();
        let several = Active::classify(&config(None), &user, None);
        let out = render_ok(
            &p,
            "app.asana.com",
            &user,
            &several,
            "ASANA_TOKEN",
            "t",
            "~/c.toml",
        );
        assert!(out.contains("  ! Active workspace: none (you belong to 2 workspaces)\n"));
        assert!(out.contains(
            "  - To pick one, pass --workspace, set ASANA_WORKSPACE, or put\n    `workspace = \"...\"` in ~/c.toml\n"
        ));

        let pref = WorkspacePref {
            value: "Nope".to_string(),
            source: WorkspaceSource::Env,
        };
        let unknown = Active::classify(&config(Some(pref)), &user, None);
        let out = render_ok(
            &p,
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
            &p,
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
    fn invalid_token_block_names_the_way_back_for_its_source() {
        let p = Palette::off();
        let out = render_invalid(
            &p,
            "app.asana.com",
            &TokenSource::Env("ASANA_TOKEN"),
            "2/12****",
        );
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 X Failed to log in to app.asana.com using token (ASANA_TOKEN)\n\
             \x20 - The token in ASANA_TOKEN is invalid.\n\
             \x20 - Token: 2/12****\n\
             \x20 - To re-authenticate, set ASANA_TOKEN to a new token from https://app.asana.com/0/my-apps\n\
             \x20 - To log in another way, unset ASANA_TOKEN and run: asana auth login\n"
        );
        let file = TokenSource::ConfigFile(PathBuf::from("/nonexistent/asana/config.toml"));
        let out = render_invalid(&p, "app.asana.com", &file, "2/12****");
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 X Failed to log in to app.asana.com using token (/nonexistent/asana/config.toml)\n\
             \x20 - The token in /nonexistent/asana/config.toml is invalid.\n\
             \x20 - Token: 2/12****\n\
             \x20 - To re-authenticate, run: asana auth login\n"
        );
        let out = render_timeout(&p, "app.asana.com", "~/.config/asana/config.toml");
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 X Timeout trying to log in to app.asana.com using token (~/.config/asana/config.toml)\n"
        );
    }

    /// gh-errors/auth-status-none: one sentence, the command in bold.
    #[test]
    fn not_logged_in_is_gh_line_with_the_command_in_bold() {
        let plain = not_logged_in(&Palette::off());
        assert!(plain.starts_with("You are not logged in to "), "{plain}");
        assert!(
            plain.ends_with(". To log in, run: asana auth login"),
            "{plain}"
        );
        assert!(plain.chars().count() < 80, "{plain}");
        let painted = not_logged_in(&Palette::new(true));
        assert!(
            painted.ends_with(". To log in, run: \x1b[0;1;39masana auth login\x1b[0m"),
            "{painted}"
        );
    }

    #[test]
    fn quoted_lists_read_like_gh_scopes() {
        assert_eq!(quoted_list(["a", "b c"]), "'a', 'b c'");
        assert_eq!(quoted_list([]), "none");
    }

    /// The help prose is hand-wrapped short of 78 columns (the help piece's
    /// rule, so it reads the same on an 80-column terminal), and the
    /// one-line `about` fits after `status:` on `asana auth --help`.
    #[test]
    fn help_prose_fits_a_narrow_screen() {
        for line in LONG_ABOUT.lines().chain(EXAMPLES.lines()) {
            assert!(line.chars().count() <= 78, "too long: {line}");
        }
        assert!(LONG_ABOUT.ends_with(".\n"));
        let about = "Display the active account and authentication state";
        assert!(LONG_ABOUT.starts_with(about));
        assert!(format!("  status:        {about}").chars().count() <= 78);
    }
}
