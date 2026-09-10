//! Everything a command needs: the API client, the resolved identity and
//! workspace (fetched lazily and cached), and output settings.

use std::io::IsTerminal;
use std::sync::OnceLock;

use anyhow::{bail, Result};

use crate::api::{Client, User, Workspace, USER_FIELDS};
use crate::config::{Config, WORKSPACE_ENV};
use crate::render::Palette;

pub struct Ctx {
    pub api: Client,
    pub config: Config,
    me: OnceLock<User>,
    workspace: OnceLock<Workspace>,
    /// Emit ANSI colour on stdout.
    pub color: bool,
    /// `--no-color` was given: colour is off on every stream, whatever
    /// its terminal-ness (`stderr_palette` reads it).
    pub no_color: bool,
    /// Terminal width in columns (or `COLUMNS`, or 80).
    pub width: usize,
    /// stdout is a terminal.
    pub is_tty: bool,
}

impl Ctx {
    pub fn new(
        api: Client,
        config: Config,
        color: bool,
        no_color: bool,
        width: usize,
        is_tty: bool,
    ) -> Self {
        Self {
            api,
            config,
            me: OnceLock::new(),
            workspace: OnceLock::new(),
            color,
            no_color,
            width,
            is_tty,
        }
    }

    /// The palette for stdout: `color` as a `Palette`.
    #[allow(dead_code)]
    pub fn palette(&self) -> Palette {
        Palette::new(self.color)
    }

    /// The palette for status lines on stderr, from stderr's own
    /// terminal-ness (never stdout's): `asana task complete 2>log` keeps the
    /// log clean, and `--no-color`, `NO_COLOR` and `TERM=dumb` switch it off
    /// as usual.
    #[allow(dead_code)]
    pub fn stderr_palette(&self) -> Palette {
        Palette::for_stream(std::io::stderr().is_terminal(), self.no_color)
    }

    /// The authenticated user, fetched once.
    pub async fn me(&self) -> Result<User> {
        if let Some(me) = self.me.get() {
            return Ok(me.clone());
        }
        let me: User = self
            .api
            .get("/users/me", &[("opt_fields", USER_FIELDS)])
            .await?;
        let _ = self.me.set(me.clone());
        Ok(me)
    }

    /// The workspace every command operates in, resolved once from
    /// `--workspace` / `ASANA_WORKSPACE` / config / the only workspace you have.
    pub async fn workspace(&self) -> Result<Workspace> {
        if let Some(ws) = self.workspace.get() {
            return Ok(ws.clone());
        }
        let me = self.me().await?;
        let ws = match &self.config.workspace {
            Some(pref) => {
                let wanted = pref.value.trim();
                let found = me.workspaces.iter().find(|w| {
                    w.gid == wanted
                        || w.name
                            .as_deref()
                            .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
                });
                match found {
                    Some(w) => w.clone(),
                    None => {
                        let mut msg = format!(
                            "workspace \"{wanted}\" (from the {}) is not one you belong to",
                            pref.source
                        );
                        if !me.workspaces.is_empty() {
                            msg.push_str("; your workspaces are:");
                            for w in &me.workspaces {
                                msg.push_str(&format!("\n  {} ({})", w.display(), w.gid));
                            }
                        }
                        bail!(msg);
                    }
                }
            }
            None => match me.workspaces.as_slice() {
                [only] => only.clone(),
                [] => bail!("this token does not belong to any workspace"),
                many => {
                    let mut msg = format!(
                        "you belong to {} workspaces; pick one with --workspace, {WORKSPACE_ENV}, or `workspace = \"...\"` in {}:",
                        many.len(),
                        crate::config::tilde(&self.config.config_path)
                    );
                    for w in many {
                        msg.push_str(&format!("\n  {} ({})", w.display(), w.gid));
                    }
                    bail!(msg);
                }
            },
        };
        let _ = self.workspace.set(ws.clone());
        Ok(ws)
    }
}
