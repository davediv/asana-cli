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
//! The `Workspaces:` line is there to choose from, so it is left out when it
//! would only repeat the active one. `--json [fields]` prints the same facts
//! as one object (`host`, `login`, `email`, `workspace`, `workspaces`,
//! `tokenSource`, `token`), the token masked unless `--show-token` is given,
//! as gh's `--json hosts` does; `--jq` filters it.
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
use serde_json::{json, Value};

use crate::api::{ApiError, User, Workspace};
use crate::cli::SilentExit;
use crate::cmd::{not_logged_in, Prevalidate};
use crate::config::{
    self, mask_token, tilde, Config, ConfigError, TokenSource, PAT_URL, WORKSPACE_ENV,
};
use crate::ctx::Ctx;
use crate::render::json::{self, JsonArgs};
use crate::render::Palette;

const EXIT_ERROR: i32 = 1;
const EXIT_AUTH: i32 = 4;

/// The keys of the `--json` object, what `--help` lists under JSON FIELDS.
const JSON_FIELDS: &[&str] = &[
    "email",
    "host",
    "login",
    "token",
    "tokenSource",
    "workspace",
    "workspaces",
];

/// gh's description, said of one host and one token; the `about` alone is
/// what `asana auth --help` lists, short enough to sit after `status:` on
/// an 80-column screen. Hand-wrapped short of 78 columns like the rest of
/// the help prose, and ending without a newline so one blank line sits
/// before USAGE, as on `gh auth status --help`.
const LONG_ABOUT: &str = "\
Display the active account and authentication state.

The token is checked against Asana and any issues are included in the
output, together with where it was read from (ASANA_TOKEN, ASANA_ACCESS_TOKEN
or ~/.config/asana/config.toml) and the active workspace, which every command
that needs one will use.

If there is no token, or Asana rejects it, the command exits with 4 and
reports on stderr, with or without `--json`.

To change the active workspace, pass `--workspace`, set `ASANA_WORKSPACE`, or
put `workspace = \"...\"` in ~/.config/asana/config.toml.

To change the account, see `asana auth login`.

For more information about output formatting, see `asana help formatting`.";

const EXAMPLES: &str = "\
# Display authentication status for the account and workspace in use
$ asana auth status

# Display authentication status with another workspace active
$ asana auth status --workspace \"Acme Corp\"

# Display the token in plain text
$ asana auth status --show-token

# Format authentication status as JSON
$ asana auth status --json

# Include the plain text token in the JSON output
$ asana auth status --json --show-token

# Print the gid of the active workspace
$ asana auth status --json workspace --jq '.workspace.gid'";

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

    #[command(flatten)]
    pub json: JsonArgs,
}

impl Prevalidate for Args {
    const JSON_FIELDS: &'static [&'static str] = JSON_FIELDS;

    /// gh checks the flags first (an unknown `--json` field or a lone
    /// `--jq` is a usage error, exit 1, token or not). Then no token
    /// anywhere is this command's answer, not a failure to set it up: gh's
    /// one not-logged-in line, exit 4. Anything else (a token, an
    /// unreadable or malformed config file) is left to `config::load`,
    /// which reports it the way every command does.
    fn prevalidate(&self, _interactive: bool) -> Result<()> {
        self.json.check(JSON_FIELDS)?;
        match config::load(None) {
            Err(ConfigError::MissingToken(_)) => {
                eprintln!("{}", not_logged_in(&early_stderr_palette()));
                Err(SilentExit(EXIT_AUTH).into())
            }
            _ => Ok(()),
        }
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let palette = ctx.palette();
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
            if args.json.is_json() {
                let value = status_json(&host, &me, &active, &source.label(), &token);
                return json::print(&value, &args.json, JSON_FIELDS);
            }
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
                    render_invalid(&ctx.stderr_palette(), &host, source, &token)
                );
                Err(SilentExit(EXIT_AUTH).into())
            }
            Some(ApiError::Network(e)) if e.is_timeout() => {
                eprint!(
                    "{}",
                    render_timeout(&ctx.stderr_palette(), &host, &source.label())
                );
                Err(SilentExit(EXIT_ERROR).into())
            }
            _ => Err(err),
        },
    }
}

/// The palette for the not-logged-in line, which `Prevalidate` prints
/// before `Ctx` (and its `stderr_palette`) exists: stderr's own
/// terminal-ness, and the `--no-color` flag read from the command line the
/// way `main` reads it for its own stderr palette. Everything `run` writes
/// on stderr uses `ctx.stderr_palette()` instead.
fn early_stderr_palette() -> Palette {
    let no_color_flag = std::env::args_os().any(|a| a == "--no-color");
    Palette::for_stream(std::io::stderr().is_terminal(), no_color_flag)
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
    // The list is there to choose from (gh's token scopes are the same kind
    // of line), so it is left out when it would only repeat the active
    // workspace, or when there is nothing in it and the `!` line already
    // said so.
    if lists_workspaces(me, active) {
        let _ = writeln!(
            out,
            "  - Workspaces: {}",
            p.bold(&quoted_list(me.workspaces.iter().map(Workspace::display)))
        );
    }
    out
}

/// Whether the `Workspaces:` line adds anything: several to choose from, or
/// one that is not the active one.
fn lists_workspaces(me: &User, active: &Active) -> bool {
    match active {
        Active::Resolved(_) => me.workspaces.len() > 1,
        _ => !me.workspaces.is_empty(),
    }
}

/// The `--json` object: the facts of the `✓` block, keyed the way gh keys
/// its `hosts` entries (`login`, `tokenSource`), the workspaces as
/// `{gid, name}` pairs and `workspace` null when none is active. `token` is
/// whatever the human line would show: masked, or plain with
/// `--show-token`.
fn status_json(host: &str, me: &User, active: &Active, source: &str, token: &str) -> Value {
    let ws = |w: &Workspace| json!({ "gid": w.gid, "name": w.name });
    let workspace = match active {
        Active::Resolved(w) => ws(w),
        _ => Value::Null,
    };
    json!({
        "host": host,
        "login": me.display(),
        "email": me.email,
        "workspace": workspace,
        "workspaces": me.workspaces.iter().map(ws).collect::<Vec<_>>(),
        "tokenSource": source,
        "token": token,
    })
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
        // The `!` line said there are none; no `Workspaces: none` after it.
        assert!(out.ends_with("  - Token: t\n"), "{out}");

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

    /// The `Workspaces:` line is a list to choose from: it goes when it
    /// would only repeat `Active workspace`, and stays whenever there is a
    /// choice, or one workspace that is not the active one.
    #[test]
    fn workspaces_line_is_left_out_when_it_repeats_the_active_one() {
        let p = Palette::off();
        let one = User {
            workspaces: vec![ws("10", "Personal")],
            ..me()
        };
        let active = Active::Resolved(ws("10", "Personal"));
        let out = render_ok(
            &p,
            "app.asana.com",
            &one,
            &active,
            "ASANA_TOKEN",
            "t",
            "~/c.toml",
        );
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 ✓ Logged in to app.asana.com account Dave Div (ASANA_TOKEN)\n\
             \x20 - Active workspace: Personal\n\
             \x20 - Email: dave@example.com\n\
             \x20 - Token: t\n"
        );
        assert!(!lists_workspaces(&one, &active));
        // Two to choose from: listed.
        assert!(lists_workspaces(&me(), &active));
        // One, but not the active one: listed, so the `!` line's advice
        // has something to point at.
        let unknown = Active::Unknown {
            value: "Nope".to_string(),
            source: "ASANA_WORKSPACE".to_string(),
        };
        assert!(lists_workspaces(&one, &unknown));
        let out = render_ok(
            &p,
            "app.asana.com",
            &one,
            &unknown,
            "ASANA_TOKEN",
            "t",
            "~/c.toml",
        );
        assert!(out.ends_with("  - Workspaces: 'Personal'\n"), "{out}");
        // Nothing at all: the `!` line already says so.
        let lonely = User {
            workspaces: Vec::new(),
            ..me()
        };
        assert!(!lists_workspaces(&lonely, &Active::None));
    }

    /// `--json`: one object with the facts of the `✓` block, gh's key
    /// style, the token exactly as the human line would show it.
    #[test]
    fn json_object_carries_the_status_facts() {
        let user = me();
        let active = Active::Resolved(ws("10", "Personal"));
        let value = status_json("app.asana.com", &user, &active, "ASANA_TOKEN", "2/12****");
        assert_eq!(
            value,
            serde_json::json!({
                "host": "app.asana.com",
                "login": "Dave Div",
                "email": "dave@example.com",
                "workspace": { "gid": "10", "name": "Personal" },
                "workspaces": [
                    { "gid": "10", "name": "Personal" },
                    { "gid": "20", "name": "Acme Corp" }
                ],
                "tokenSource": "ASANA_TOKEN",
                "token": "2/12****"
            })
        );
        // Every key is advertised, and nothing else is.
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut advertised = JSON_FIELDS.to_vec();
        advertised.sort_unstable();
        assert_eq!(keys, advertised);
        // No active workspace is null, never a missing key.
        let value = status_json(
            "app.asana.com",
            &user,
            &Active::Several(2),
            "ASANA_TOKEN",
            "t",
        );
        assert_eq!(value["workspace"], serde_json::Value::Null);
        assert_eq!(value["workspaces"].as_array().map(Vec::len), Some(2));
        // A user without an email or a name still serialises.
        let bare = User {
            gid: "7".to_string(),
            name: None,
            email: None,
            workspaces: Vec::new(),
        };
        let value = status_json("app.asana.com", &bare, &Active::None, "~/c.toml", "t");
        assert_eq!(value["login"], "7");
        assert_eq!(value["email"], serde_json::Value::Null);
        assert_eq!(value["tokenSource"], "~/c.toml");
    }

    /// The whole flag surface parses, and `--json` takes its field list
    /// the way gh's does: the next word, never a following flag.
    #[test]
    fn json_flags_parse_and_are_checked_before_the_token() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: Args,
        }
        let parse = |argv: &[&str]| {
            Cli::try_parse_from(std::iter::once("asana").chain(argv.iter().copied()))
                .map(|c| c.args)
        };
        let plain = parse(&[]).unwrap();
        assert!(!plain.json.is_json() && !plain.show_token);
        let all = parse(&["--json", "--show-token"]).unwrap();
        assert!(all.json.is_json() && all.show_token);
        assert!(all.json.fields().is_empty());
        let some = parse(&["--json", "workspace,login", "-q", ".workspace.gid"]).unwrap();
        assert_eq!(some.json.fields(), vec!["workspace", "login"]);
        assert_eq!(some.json.jq.as_deref(), Some(".workspace.gid"));
        assert!(some.json.check(JSON_FIELDS).is_ok());
        // gh's checks, from the shared helper, run before the token is read.
        let bad = parse(&["--json", "scopes"]).unwrap();
        let err = bad.json.check(JSON_FIELDS).unwrap_err().to_string();
        assert!(
            err.starts_with("Unknown JSON field: \"scopes\"\nAvailable fields:\n  email\n"),
            "{err}"
        );
        let lone = parse(&["--jq", ".login"]).unwrap();
        assert_eq!(
            lone.json.check(JSON_FIELDS).unwrap_err().to_string(),
            "cannot use `--jq` without specifying `--json`"
        );
        // A dotted field is judged by its first segment.
        assert!(parse(&["--json", "workspace.gid"])
            .unwrap()
            .json
            .check(JSON_FIELDS)
            .is_ok());
        // The help lists them sorted, on one line.
        assert_eq!(
            json::format_fields(JSON_FIELDS, "  ", 80),
            "  email, host, login, token, tokenSource, workspace, workspaces\n"
        );
    }

    /// The help prose is hand-wrapped short of 78 columns (the help piece's
    /// rule, so it reads the same on an 80-column terminal), and the
    /// one-line `about` fits after `status:` on `asana auth --help`.
    #[test]
    fn help_prose_fits_a_narrow_screen() {
        for line in LONG_ABOUT.lines().chain(EXAMPLES.lines()) {
            assert!(line.chars().count() <= 78, "too long: {line}");
        }
        assert!(LONG_ABOUT.ends_with('.'), "one blank line before USAGE");
        let about = "Display the active account and authentication state";
        assert!(LONG_ABOUT.starts_with(about));
        assert!(format!("  status:        {about}").chars().count() <= 78);
    }
}
