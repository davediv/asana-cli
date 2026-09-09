//! Where the token and the workspace preference come from.
//!
//! Token: `ASANA_TOKEN`, then `ASANA_ACCESS_TOKEN`, then `token` in
//! `$XDG_CONFIG_HOME/asana/config.toml` (`~/.config/asana/config.toml`).
//! Workspace: `--workspace`, then `ASANA_WORKSPACE` (gid or name), then
//! `workspace` in the config file; otherwise the only workspace the user has
//! (resolved lazily by `Ctx`).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const TOKEN_ENV: &str = "ASANA_TOKEN";
pub const TOKEN_ENV_ALT: &str = "ASANA_ACCESS_TOKEN";
pub const WORKSPACE_ENV: &str = "ASANA_WORKSPACE";
pub const PAT_URL: &str = "https://app.asana.com/0/my-apps";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No token anywhere. Exit code 4.
    #[error("To get started with the Asana CLI, populate the {TOKEN_ENV} environment variable with a personal access token.\nCreate one at {PAT_URL}, or put `token = \"...\"` in {0}.")]
    MissingToken(String),
    #[error("could not read {path}: {source}")]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse {path}: {source}")]
    Malformed {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    Env(&'static str),
    ConfigFile(PathBuf),
}

impl fmt::Display for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenSource::Env(name) => write!(f, "{name} environment variable"),
            TokenSource::ConfigFile(path) => write!(f, "{}", tilde(path)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSource {
    Flag,
    Env,
    ConfigFile(PathBuf),
}

impl fmt::Display for WorkspaceSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkspaceSource::Flag => write!(f, "--workspace flag"),
            WorkspaceSource::Env => write!(f, "{WORKSPACE_ENV} environment variable"),
            WorkspaceSource::ConfigFile(path) => write!(f, "{}", tilde(path)),
        }
    }
}

/// A workspace preference before it is matched against the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePref {
    /// A gid or a name, as the user typed it.
    pub value: String,
    pub source: WorkspaceSource,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub token: String,
    pub token_source: TokenSource,
    pub workspace: Option<WorkspacePref>,
    /// The config file path, whether or not it exists (for messages).
    pub config_path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    token: Option<String>,
    workspace: Option<String>,
}

/// `$XDG_CONFIG_HOME/asana/config.toml`, falling back to `~/.config/asana/config.toml`.
pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("asana").join("config.toml")
}

/// Render a path with the home directory as `~` for messages.
pub fn tilde(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn read_file(path: &Path) -> Result<FileConfig, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Malformed {
            path: path.to_path_buf(),
            source,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(source) => Err(ConfigError::Unreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Resolve the token and the workspace preference. `workspace_flag` is the
/// value of `--workspace`, which wins over everything else.
pub fn load(workspace_flag: Option<String>) -> Result<Config, ConfigError> {
    let config_path = config_path();
    let file = read_file(&config_path)?;

    let (token, token_source) = if let Some(t) = non_empty_env(TOKEN_ENV) {
        (t, TokenSource::Env(TOKEN_ENV))
    } else if let Some(t) = non_empty_env(TOKEN_ENV_ALT) {
        (t, TokenSource::Env(TOKEN_ENV_ALT))
    } else if let Some(t) = file
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        (t.to_string(), TokenSource::ConfigFile(config_path.clone()))
    } else {
        return Err(ConfigError::MissingToken(tilde(&config_path)));
    };

    let workspace = if let Some(v) = workspace_flag
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        Some(WorkspacePref {
            value: v,
            source: WorkspaceSource::Flag,
        })
    } else if let Some(v) = non_empty_env(WORKSPACE_ENV) {
        Some(WorkspacePref {
            value: v,
            source: WorkspaceSource::Env,
        })
    } else {
        file.workspace
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| WorkspacePref {
                value: v.to_string(),
                source: WorkspaceSource::ConfigFile(config_path.clone()),
            })
    };

    Ok(Config {
        token,
        token_source,
        workspace,
        config_path,
    })
}

/// `2/12************` — the first few characters, then stars, like `gh auth status`.
pub fn mask_token(token: &str) -> String {
    let keep = token.chars().take(4).collect::<String>();
    let rest = token.chars().count().saturating_sub(keep.chars().count());
    format!("{keep}{}", "*".repeat(rest.max(8)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_all_but_prefix() {
        assert_eq!(mask_token("2/1234567890/abcdef"), "2/12***************");
        assert_eq!(mask_token("ab"), "ab********");
    }
}
