//! `asana auth status`: who the token belongs to, where it came from, and
//! which workspace commands will use.
//!
//! ```text
//! app.asana.com
//!   ✓ Logged in to app.asana.com account Dave Div (ASANA_TOKEN)
//!   - Active workspace: Personal (ASANA_WORKSPACE)
//!   - Email: dave@example.com
//!   - Token: 2/12************************************
//!   - Workspaces: 'Personal', 'Acme Corp'
//! ```
//!
//! Every line names where its fact came from, in parentheses: the token's
//! source after the account, the preference that chose the workspace after
//! its name (`the only one` when there was nothing to choose). The
//! `Workspaces:` line is there to choose from, so it is left out when it
//! would only repeat the active one. `--json [fields]` prints the same
//! facts as one object (`host`, `login`, `email`, `workspace`,
//! `workspaces`, `tokenSource`, `workspaceSource`, `token`), the token
//! masked unless `--show-token` is given; `--jq` filters it.
//!
//! The states it reports:
//!
//! - no token anywhere: one line (`You are not logged in to app.asana.com.
//!   To log in, run: asana auth login`), on stderr, exit 4, the same
//!   sentence `auth logout` and `auth token` print. It is decided in
//!   `Prevalidate`, before `main` reads the config, so the state is this
//!   command's answer rather than the generic "set ASANA_TOKEN" error every
//!   other command gets;
//! - a token Asana rejects: the same block with an `X` line, on stderr,
//!   exit 4;
//! - a workspace that cannot be resolved: a `!` line with how to pick one,
//!   exit 0 (the token works; only the preference is off).
//!
//! Every block is wrapped to the screen, so a long name or a long config
//! path never runs off an 80-column terminal.

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
use crate::render::text::{wrap_line, PROSE_WIDTH};
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
    "workspaceSource",
    "workspaces",
];

/// The description this screen opens with; the `about` alone is what
/// `asana auth --help` lists, short enough to sit after `status:` on an
/// 80-column screen. Hand-wrapped short of 78 columns like the rest of the
/// help prose, and ending without a newline so exactly one blank line sits
/// between it and USAGE.
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

    /// The flags are checked first (an unknown `--json` field or a lone
    /// `--jq` is a usage error, exit 1, token or not). Then no token
    /// anywhere is this command's answer, not a failure to set it up: the
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
    let host = config::host();
    let token = if args.show_token {
        ctx.config.token.clone()
    } else {
        mask_token(&ctx.config.token)
    };
    let source = &ctx.config.token_source;
    let config_path = tilde(&ctx.config.config_path);
    let stdout = ctx.palette();
    let stderr = ctx.stderr_palette();
    let block = Block {
        p: &stdout,
        host: &host,
        source,
        token: &token,
        config_path: &config_path,
        width: ctx.width.min(PROSE_WIDTH),
    };

    match ctx.me().await {
        Ok(me) => {
            let resolved = ctx.workspace().await.ok();
            let active = Active::classify(&ctx.config, &me, resolved);
            if args.json.is_json() {
                let value = status_json(&host, &me, &active, &source.label(), &token);
                return json::print(&value, &args.json, JSON_FIELDS);
            }
            print!("{}", block.ok(&me, &active));
            Ok(())
        }
        // A token Asana will not take, and a network that never answered,
        // are failures: they go to stderr, leaving stdout empty.
        Err(err) => match err.downcast_ref::<ApiError>() {
            Some(ApiError::Unauthorized { .. }) => {
                eprint!(
                    "{}",
                    Block {
                        p: &stderr,
                        ..block
                    }
                    .invalid()
                );
                Err(SilentExit(EXIT_AUTH).into())
            }
            Some(ApiError::Network(e)) if e.is_timeout() => {
                eprint!(
                    "{}",
                    Block {
                        p: &stderr,
                        ..block
                    }
                    .timeout()
                );
                Err(SilentExit(EXIT_ERROR).into())
            }
            _ => Err(err),
        },
    }
}

/// One status block: the palette and the screen it is written on, and the
/// facts that are the same whatever Asana answers.
struct Block<'a> {
    p: &'a Palette,
    host: &'a str,
    /// Where the token was read from.
    source: &'a TokenSource,
    /// The token as the block shows it: masked, or plain under
    /// `--show-token`.
    token: &'a str,
    /// The config file, for the advice under a `!` line.
    config_path: &'a str,
    /// The width to wrap to: the screen, never wider than prose is allowed
    /// to be, so the advice reads the same on a wide terminal as it does
    /// on an 80-column one.
    width: usize,
}

impl Block<'_> {
    /// One line of the block: `  <mark> <text>`, wrapped with the
    /// continuation hanging under the text rather than under the mark.
    /// Colour is invisible to the wrapper, so a painted value never costs
    /// a line.
    fn line(&self, out: &mut String, mark: &str, text: &str) {
        for line in wrap_line(text, self.width, &format!("  {mark} "), "    ") {
            let _ = writeln!(out, "{line}");
        }
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

/// `'Personal', 'Acme Corp'`: a list of names to pick from, on one line.
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
    /// The workspace commands will use, and the preference that chose it
    /// (`None` when nothing was set and it is the only one there is).
    Resolved {
        workspace: Workspace,
        source: Option<String>,
    },
    /// A preference that matched none of the user's workspaces.
    Unknown { value: String, source: String },
    /// Nothing configured and more than one to choose from.
    Several(usize),
    /// The token belongs to no workspace at all.
    None,
}

impl Active {
    fn classify(config: &Config, me: &User, resolved: Option<Workspace>) -> Self {
        let pref = config.workspace.as_ref();
        if let Some(workspace) = resolved {
            return Active::Resolved {
                workspace,
                source: pref.map(|p| p.source.label()),
            };
        }
        match pref {
            Some(pref) => Active::Unknown {
                value: pref.value.clone(),
                source: pref.source.label(),
            },
            None if me.workspaces.is_empty() => Active::None,
            None => Active::Several(me.workspaces.len()),
        }
    }

    /// Where the preference that names the workspace came from, whether or
    /// not it resolved; `None` when nothing was configured.
    fn source(&self) -> Option<&str> {
        match self {
            Active::Resolved { source, .. } => source.as_deref(),
            Active::Unknown { source, .. } => Some(source),
            _ => Option::None,
        }
    }
}

impl Block<'_> {
    /// The block for a token Asana accepted: who it belongs to, which
    /// workspace commands will use, and the token itself.
    fn ok(&self, me: &User, active: &Active) -> String {
        let (p, host) = (self.p, self.host);
        let mut out = String::new();
        let _ = writeln!(out, "{}", p.bold(host));
        self.line(
            &mut out,
            &p.success_icon(),
            &format!(
                "Logged in to {host} account {} ({})",
                p.bold(me.display()),
                self.source.label()
            ),
        );
        let pick = format!(
            "To pick one, pass --workspace, set {WORKSPACE_ENV}, or put `workspace = \"...\"` in {}",
            self.config_path
        );
        match active {
            // The workspace, and the preference that chose it, the way the
            // account line names where the token came from; nothing
            // configured and one workspace to have means there was nothing
            // to choose.
            Active::Resolved { workspace, source } => {
                let why = source.as_deref().unwrap_or("the only one");
                self.line(
                    &mut out,
                    "-",
                    &format!("Active workspace: {} ({why})", p.bold(workspace.display())),
                );
            }
            Active::Unknown { value, source } => {
                self.line(
                    &mut out,
                    &p.warning_icon(),
                    &format!(
                        "Active workspace: {} ({source}) is not one you belong to",
                        p.bold(&format!("\"{value}\""))
                    ),
                );
                self.line(&mut out, "-", &pick);
            }
            Active::Several(n) => {
                self.line(
                    &mut out,
                    &p.warning_icon(),
                    &format!(
                        "Active workspace: {} (you belong to {n} workspaces)",
                        p.bold("none")
                    ),
                );
                self.line(&mut out, "-", &pick);
            }
            Active::None => {
                self.line(
                    &mut out,
                    &p.warning_icon(),
                    &format!(
                        "Active workspace: {} (this token does not belong to any workspace)",
                        p.bold("none")
                    ),
                );
            }
        }
        if let Some(email) = me.email.as_deref().filter(|e| !e.is_empty()) {
            self.line(&mut out, "-", &format!("Email: {}", p.bold(email)));
        }
        self.line(&mut out, "-", &format!("Token: {}", p.bold(self.token)));
        // The list is there to choose from, so it is left out when it
        // would only repeat the active workspace, or when there is nothing
        // in it and the `!` line already said so.
        if lists_workspaces(me, active) {
            self.line(
                &mut out,
                "-",
                &format!(
                    "Workspaces: {}",
                    p.bold(&quoted_list(me.workspaces.iter().map(Workspace::display)))
                ),
            );
        }
        out
    }
}

/// Whether the `Workspaces:` line adds anything: several to choose from, or
/// one that is not the active one.
fn lists_workspaces(me: &User, active: &Active) -> bool {
    match active {
        Active::Resolved { .. } => me.workspaces.len() > 1,
        _ => !me.workspaces.is_empty(),
    }
}

/// The `--json` object: the facts of the `✓` block, one camelCase key
/// each, the workspaces as `{gid, name}` pairs, `workspace` null when none
/// is active and `workspaceSource` null when nothing chose it. `token` is
/// whatever the human line would show: masked, or plain with
/// `--show-token`.
fn status_json(host: &str, me: &User, active: &Active, source: &str, token: &str) -> Value {
    let ws = |w: &Workspace| json!({ "gid": w.gid, "name": w.name });
    let workspace = match active {
        Active::Resolved { workspace, .. } => ws(workspace),
        _ => Value::Null,
    };
    json!({
        "host": host,
        "login": me.display(),
        "email": me.email,
        "workspace": workspace,
        "workspaceSource": active.source(),
        "workspaces": me.workspaces.iter().map(ws).collect::<Vec<_>>(),
        "tokenSource": source,
        "token": token,
    })
}

impl Block<'_> {
    /// The block for a token Asana rejects: the same shape as the `✓` one,
    /// with the way back that fits where the token came from — `asana auth
    /// login` rewrites the config file, while a variable has to be set again
    /// (or unset, so that login can take over).
    fn invalid(&self) -> String {
        let (p, host) = (self.p, self.host);
        let label = self.source.label();
        let mut out = String::new();
        let _ = writeln!(out, "{}", p.bold(host));
        self.line(
            &mut out,
            &p.failure_icon(),
            &format!("Failed to log in to {host} using token ({label})"),
        );
        self.line(&mut out, "-", &format!("The token in {label} is invalid."));
        self.line(&mut out, "-", &format!("Token: {}", p.bold(self.token)));
        match self.source {
            TokenSource::ConfigFile(_) => {
                self.line(
                    &mut out,
                    "-",
                    &format!("To re-authenticate, run: {}", p.bold("asana auth login")),
                );
            }
            TokenSource::Env(_) => {
                self.line(
                    &mut out,
                    "-",
                    &format!("To re-authenticate, set {label} to a new token from {PAT_URL}"),
                );
                self.line(
                    &mut out,
                    "-",
                    &format!(
                        "To log in another way, unset {label} and run: {}",
                        p.bold("asana auth login")
                    ),
                );
            }
        }
        out
    }

    /// Asana never answered: one line, and nothing about the token, which
    /// was never checked.
    fn timeout(&self) -> String {
        let (p, host) = (self.p, self.host);
        let mut out = String::new();
        let _ = writeln!(out, "{}", p.bold(host));
        self.line(
            &mut out,
            &p.failure_icon(),
            &format!(
                "Timeout trying to log in to {host} using token ({})",
                self.source.label()
            ),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{WorkspacePref, WorkspaceSource};
    use std::path::PathBuf;

    const WIDE: usize = PROSE_WIDTH;

    const HOST: &str = "app.asana.com";
    const ENV: TokenSource = TokenSource::Env("ASANA_TOKEN");

    /// One `✓` block: the same host and token source throughout, so a test
    /// says only what it is about.
    fn ok_block(
        p: &Palette,
        me: &User,
        active: &Active,
        token: &str,
        path: &str,
        width: usize,
    ) -> String {
        Block {
            p,
            host: HOST,
            source: &ENV,
            token,
            config_path: path,
            width,
        }
        .ok(me, active)
    }

    /// The block for a rejected token, from `source`.
    fn invalid_block(p: &Palette, source: &TokenSource, token: &str, width: usize) -> String {
        Block {
            p,
            host: HOST,
            source,
            token,
            config_path: "~/c.toml",
            width,
        }
        .invalid()
    }

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

    fn pref(value: &str, source: WorkspaceSource) -> WorkspacePref {
        WorkspacePref {
            value: value.to_string(),
            source,
        }
    }

    fn resolved(gid: &str, name: &str, source: Option<&str>) -> Active {
        Active::Resolved {
            workspace: ws(gid, name),
            source: source.map(str::to_string),
        }
    }

    #[test]
    fn happy_path_names_every_fact_and_where_it_came_from() {
        let p = Palette::off();
        let active = resolved("10", "Personal", Some("ASANA_WORKSPACE"));
        let out = ok_block(
            &p,
            &me(),
            &active,
            "2/12***************",
            "~/.config/asana/config.toml",
            WIDE,
        );
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 ✓ Logged in to app.asana.com account Dave Div (ASANA_TOKEN)\n\
             \x20 - Active workspace: Personal (ASANA_WORKSPACE)\n\
             \x20 - Email: dave@example.com\n\
             \x20 - Token: 2/12***************\n\
             \x20 - Workspaces: 'Personal', 'Acme Corp'\n"
        );
    }

    /// Nothing configured and one workspace to have: the parenthesis says
    /// so, which is why no `Workspaces:` line follows.
    #[test]
    fn one_workspace_and_no_preference_says_the_only_one() {
        let p = Palette::off();
        let one = User {
            workspaces: vec![ws("10", "Personal")],
            ..me()
        };
        let out = ok_block(
            &p,
            &one,
            &resolved("10", "Personal", None),
            "t",
            "~/c.toml",
            WIDE,
        );
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 ✓ Logged in to app.asana.com account Dave Div (ASANA_TOKEN)\n\
             \x20 - Active workspace: Personal (the only one)\n\
             \x20 - Email: dave@example.com\n\
             \x20 - Token: t\n"
        );
    }

    /// The host, the account and every value bold; the tick green; the
    /// labels plain.
    #[test]
    fn colour_paints_the_values_not_the_labels() {
        let p = Palette::new(true);
        let active = resolved("10", "Personal", Some("ASANA_WORKSPACE"));
        let out = ok_block(
            &p,
            &me(),
            &active,
            "2/12****",
            "~/.config/asana/config.toml",
            WIDE,
        );
        assert_eq!(
            out,
            "\x1b[0;1;39mapp.asana.com\x1b[0m\n\
             \x20 \x1b[0;32m✓\x1b[0m Logged in to app.asana.com account \x1b[0;1;39mDave Div\x1b[0m (ASANA_TOKEN)\n\
             \x20 - Active workspace: \x1b[0;1;39mPersonal\x1b[0m (ASANA_WORKSPACE)\n\
             \x20 - Email: \x1b[0;1;39mdave@example.com\x1b[0m\n\
             \x20 - Token: \x1b[0;1;39m2/12****\x1b[0m\n\
             \x20 - Workspaces: \x1b[0;1;39m'Personal', 'Acme Corp'\x1b[0m\n"
        );
    }

    /// Colour is invisible to the wrapper: a coloured line breaks at the
    /// same word as the plain one, so painting never costs a line.
    #[test]
    fn wrapping_measures_the_text_not_the_escapes() {
        let long = User {
            name: Some("Dave Div".to_string()),
            workspaces: vec![ws("10", "Engineering and Design")],
            ..me()
        };
        let active = resolved("10", "Engineering and Design", Some("--workspace"));
        let plain = ok_block(&Palette::off(), &long, &active, "2/12****", "~/c.toml", 48);
        let painted = ok_block(
            &Palette::new(true),
            &long,
            &active,
            "2/12****",
            "~/c.toml",
            48,
        );
        assert_eq!(
            plain,
            "app.asana.com\n\
             \x20 ✓ Logged in to app.asana.com account Dave Div\n\
             \x20   (ASANA_TOKEN)\n\
             \x20 - Active workspace: Engineering and Design\n\
             \x20   (--workspace)\n\
             \x20 - Email: dave@example.com\n\
             \x20 - Token: 2/12****\n"
        );
        let stripped = strip_ansi(&painted);
        assert_eq!(stripped, plain);
        // No line of either overruns the screen it was given.
        for line in stripped.lines() {
            assert!(crate::render::text::width(line) <= 48, "too wide: {line}");
        }
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn unresolved_workspace_is_a_warning_with_advice() {
        let p = Palette::off();
        let user = me();
        let several = Active::classify(&config(None), &user, None);
        let out = ok_block(&p, &user, &several, "t", "~/c.toml", WIDE);
        assert!(out.contains("  ! Active workspace: none (you belong to 2 workspaces)\n"));
        assert!(out.contains(
            "  - To pick one, pass --workspace, set ASANA_WORKSPACE, or put\n    `workspace = \"...\"` in ~/c.toml\n"
        ));

        let unknown = Active::classify(
            &config(Some(pref("Nope", WorkspaceSource::Env))),
            &user,
            None,
        );
        let out = ok_block(&p, &user, &unknown, "t", "~/c.toml", WIDE);
        assert!(out.contains(
            "  ! Active workspace: \"Nope\" (ASANA_WORKSPACE) is not one you belong to\n"
        ));

        let lonely = User {
            workspaces: Vec::new(),
            ..user.clone()
        };
        let none = Active::classify(&config(None), &lonely, None);
        let out = ok_block(&p, &lonely, &none, "t", "~/c.toml", WIDE);
        assert!(out.contains(
            "  ! Active workspace: none (this token does not belong to any workspace)\n"
        ));
        // The `!` line said there are none; no `Workspaces: none` after it.
        assert!(out.ends_with("  - Token: t\n"), "{out}");
    }

    /// A resolved workspace remembers which preference chose it, and says
    /// nothing chose it when none was set.
    #[test]
    fn classify_keeps_the_workspace_preference() {
        let user = me();
        let from_flag = Active::classify(
            &config(Some(pref("Personal", WorkspaceSource::Flag))),
            &user,
            Some(ws("10", "Personal")),
        );
        assert_eq!(from_flag.source(), Some("--workspace"));
        assert!(matches!(&from_flag, Active::Resolved { workspace, .. } if workspace.gid == "10"));
        let only = Active::classify(&config(None), &user, Some(ws("10", "Personal")));
        assert_eq!(only.source(), None);
        // An unresolved preference still names where it came from.
        let unknown = Active::classify(
            &config(Some(pref("Nope", WorkspaceSource::Env))),
            &user,
            None,
        );
        assert_eq!(unknown.source(), Some("ASANA_WORKSPACE"));
        assert_eq!(Active::Several(2).source(), None);
    }

    #[test]
    fn invalid_token_block_names_the_way_back_for_its_source() {
        let p = Palette::off();
        let out = invalid_block(&p, &ENV, "2/12****", WIDE);
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 X Failed to log in to app.asana.com using token (ASANA_TOKEN)\n\
             \x20 - The token in ASANA_TOKEN is invalid.\n\
             \x20 - Token: 2/12****\n\
             \x20 - To re-authenticate, set ASANA_TOKEN to a new token from\n\
             \x20   https://app.asana.com/0/my-apps\n\
             \x20 - To log in another way, unset ASANA_TOKEN and run: asana auth login\n"
        );
        // The URL is never broken, however narrow the screen.
        let narrow = invalid_block(&p, &TokenSource::Env("ASANA_ACCESS_TOKEN"), "2/12****", 40);
        assert!(
            narrow.contains(" https://app.asana.com/0/my-apps\n"),
            "{narrow}"
        );
        for line in narrow.lines() {
            assert!(crate::render::text::width(line) <= 40, "too wide: {line}");
        }
        let file = TokenSource::ConfigFile(PathBuf::from("/nonexistent/asana/config.toml"));
        let out = invalid_block(&p, &file, "2/12****", WIDE);
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 X Failed to log in to app.asana.com using token\n\
             \x20   (/nonexistent/asana/config.toml)\n\
             \x20 - The token in /nonexistent/asana/config.toml is invalid.\n\
             \x20 - Token: 2/12****\n\
             \x20 - To re-authenticate, run: asana auth login\n"
        );
        let out = Block {
            p: &p,
            host: HOST,
            source: &ENV,
            token: "2/12****",
            config_path: "~/c.toml",
            width: WIDE,
        }
        .timeout();
        assert_eq!(
            out,
            "app.asana.com\n\
             \x20 X Timeout trying to log in to app.asana.com using token (ASANA_TOKEN)\n"
        );
    }

    /// No token at all: one sentence, the command in bold.
    #[test]
    fn not_logged_in_is_one_line_with_the_command_in_bold() {
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
    fn quoted_lists_read_like_a_list_of_names() {
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
        let active = resolved("10", "Personal", None);
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
        let out = ok_block(&p, &one, &unknown, "t", "~/c.toml", WIDE);
        assert!(out.ends_with("  - Workspaces: 'Personal'\n"), "{out}");
        // Nothing at all: the `!` line already says so.
        let lonely = User {
            workspaces: Vec::new(),
            ..me()
        };
        assert!(!lists_workspaces(&lonely, &Active::None));
    }

    /// `--json`: one object with the facts of the `✓` block, the token
    /// exactly as the human line would show it.
    #[test]
    fn json_object_carries_the_status_facts() {
        let user = me();
        let active = resolved("10", "Personal", Some("ASANA_WORKSPACE"));
        let value = status_json("app.asana.com", &user, &active, "ASANA_TOKEN", "2/12****");
        assert_eq!(
            value,
            serde_json::json!({
                "host": "app.asana.com",
                "login": "Dave Div",
                "email": "dave@example.com",
                "workspace": { "gid": "10", "name": "Personal" },
                "workspaceSource": "ASANA_WORKSPACE",
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
        // Nothing chose the workspace: null, never a missing key.
        let value = status_json(
            "app.asana.com",
            &user,
            &resolved("10", "Personal", None),
            "ASANA_TOKEN",
            "t",
        );
        assert_eq!(value["workspaceSource"], serde_json::Value::Null);
        // No active workspace is null too, and the list still comes back.
        let value = status_json(
            "app.asana.com",
            &user,
            &Active::Several(2),
            "ASANA_TOKEN",
            "t",
        );
        assert_eq!(value["workspace"], serde_json::Value::Null);
        assert_eq!(value["workspaceSource"], serde_json::Value::Null);
        assert_eq!(value["workspaces"].as_array().map(Vec::len), Some(2));
        // A preference that resolved to nothing still says where it came from.
        let unknown = Active::Unknown {
            value: "Nope".to_string(),
            source: "ASANA_WORKSPACE".to_string(),
        };
        let value = status_json("app.asana.com", &user, &unknown, "ASANA_TOKEN", "t");
        assert_eq!(value["workspace"], serde_json::Value::Null);
        assert_eq!(value["workspaceSource"], "ASANA_WORKSPACE");
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

    /// The whole flag surface parses, and `--json` takes its field list as
    /// the next word, never a following flag.
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
        // The shared checks run before the token is read.
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
        // The help lists them sorted, and wraps them when they do not fit.
        assert_eq!(
            json::format_fields(JSON_FIELDS, "  ", 120),
            "  email, host, login, token, tokenSource, workspace, workspaceSource, workspaces\n"
        );
    }

    /// The help prose is hand-wrapped short of 78 columns, so it reads the
    /// same on an 80-column terminal, and the one-line `about` fits after
    /// `status:` on `asana auth --help`.
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
