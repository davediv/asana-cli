//! Where the token and the workspace preference come from, and where
//! `asana auth login` puts a token.
//!
//! Token: `ASANA_TOKEN`, then `ASANA_ACCESS_TOKEN`, then `token` in
//! `$XDG_CONFIG_HOME/asana/config.toml` (`~/.config/asana/config.toml`).
//! Workspace: `--workspace`, then `ASANA_WORKSPACE` (gid or name), then
//! `workspace` in the config file; otherwise the only workspace the user has
//! (resolved lazily by `Ctx`).
//!
//! Writes (`save_token`, `save_login`, `remove_token`) edit the file as a
//! `toml_edit` document, so comments, blank lines and every other key stay
//! as the user wrote them, and the file is readable only by its owner.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;
use toml_edit::{DocumentMut, Item};

pub const TOKEN_ENV: &str = "ASANA_TOKEN";
pub const TOKEN_ENV_ALT: &str = "ASANA_ACCESS_TOKEN";
pub const WORKSPACE_ENV: &str = "ASANA_WORKSPACE";
pub const PAT_URL: &str = "https://app.asana.com/0/my-apps";
/// The production host, for messages (`Logged in to app.asana.com`).
#[allow(dead_code)] // auth_status and auth_login switch to these next loop
pub const HOST: &str = "app.asana.com";
/// The config-file keys `asana auth login` writes: the token, and the
/// account's display name beside it so `auth logout` can say whose it was.
#[allow(dead_code)]
pub const TOKEN_KEY: &str = "token";
#[allow(dead_code)]
pub const USER_KEY: &str = "user";

/// The no-token message, in gh's two parts (`reference/gh-errors/no-auth.txt`):
/// what to run, then the environment variable for scripts, hand-wrapped
/// under 80 columns so the URL never breaks mid-path.
const MISSING_TOKEN: &str = "\
To get started with the Asana CLI, please run:  asana auth login
Alternatively, populate the ASANA_TOKEN environment variable with a personal
access token from https://app.asana.com/0/my-apps.";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No token anywhere (the payload is the config file that had none,
    /// for messages that want to name it). Exit code 4.
    #[error("{MISSING_TOKEN}")]
    MissingToken(String),
    #[error("could not read {}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse {}", path.display())]
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

impl TokenSource {
    /// The short label gh puts in parentheses after the account:
    /// `ASANA_TOKEN`, `ASANA_ACCESS_TOKEN` or `~/.config/asana/config.toml`.
    #[allow(dead_code)]
    pub fn label(&self) -> String {
        match self {
            TokenSource::Env(name) => (*name).to_string(),
            TokenSource::ConfigFile(path) => tilde(path),
        }
    }
}

/// The long form for prose: `ASANA_TOKEN environment variable`, or the file.
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

impl WorkspaceSource {
    /// The short label: `--workspace`, `ASANA_WORKSPACE` or the config file.
    #[allow(dead_code)]
    pub fn label(&self) -> String {
        match self {
            WorkspaceSource::Flag => "--workspace".to_string(),
            WorkspaceSource::Env => WORKSPACE_ENV.to_string(),
            WorkspaceSource::ConfigFile(path) => tilde(path),
        }
    }
}

/// The long form for prose: `--workspace flag`, `ASANA_WORKSPACE environment
/// variable`, or the file.
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

/// `ASANA_BASE_URL` when it is set and not blank (a mock server, in tests),
/// else `None` for the production API.
#[allow(dead_code)]
pub fn base_url() -> Option<String> {
    non_empty_env("ASANA_BASE_URL")
}

/// `app.asana.com`, or the host of `ASANA_BASE_URL` when one is set, so a
/// message names the server that was actually asked (`Logged in to
/// app.asana.com`, `You are not logged in to app.asana.com`).
#[allow(dead_code)]
pub fn host() -> String {
    base_url()
        .and_then(|b| url::Url::parse(&b).ok())
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| HOST.to_string())
}

/// What `asana auth login` stored in the config file: the token and, when
/// login recorded it, the account's display name.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct Login {
    pub token: String,
    pub user: Option<String>,
}

/// The login stored in the config file, if it holds a non-empty `token`.
#[allow(dead_code)]
pub fn stored_login() -> anyhow::Result<Option<Login>> {
    stored_login_at(&config_path())
}

/// `stored_login` for a given file.
#[allow(dead_code)]
pub fn stored_login_at(path: &Path) -> anyhow::Result<Option<Login>> {
    let Some(doc) = read_document(path)? else {
        return Ok(None);
    };
    Ok(string_key(&doc, TOKEN_KEY).map(|token| Login {
        token,
        user: string_key(&doc, USER_KEY),
    }))
}

/// Store `token` in the config file (created readable only by the owner)
/// and return the file's path. Every other key, comment and blank line
/// stays where it was; a `user` left by an earlier login is dropped, since
/// it named the old token's account. Use `save_login` to record the name.
#[allow(dead_code)]
pub fn save_token(token: &str) -> anyhow::Result<PathBuf> {
    save_login(token, None)
}

/// `save_token` that also records the account's display name beside the
/// token (`user = "..."`), for `auth logout` to name.
#[allow(dead_code)]
pub fn save_login(token: &str, user: Option<&str>) -> anyhow::Result<PathBuf> {
    let path = config_path();
    save_login_at(&path, token, user)?;
    Ok(path)
}

/// `save_login` for a given file.
#[allow(dead_code)]
pub fn save_login_at(path: &Path, token: &str, user: Option<&str>) -> anyhow::Result<()> {
    let mut doc = read_document(path)?.unwrap_or_default();
    set_string(&mut doc, TOKEN_KEY, token);
    match user {
        Some(user) => set_string(&mut doc, USER_KEY, user),
        None => {
            doc.remove(USER_KEY);
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", tilde(dir)))?;
    }
    write_private(path, &doc.to_string())
        .with_context(|| format!("could not write {}", tilde(path)))?;
    Ok(())
}

/// Remove the stored `token` (and `user`) from the config file. `true` when
/// a token was there; a file without one is left alone. A file with nothing
/// else in it, which `login` created, is removed.
#[allow(dead_code)]
pub fn remove_token() -> anyhow::Result<bool> {
    remove_token_at(&config_path())
}

/// `remove_token` for a given file.
#[allow(dead_code)]
pub fn remove_token_at(path: &Path) -> anyhow::Result<bool> {
    let Some(mut doc) = read_document(path)? else {
        return Ok(false);
    };
    if string_key(&doc, TOKEN_KEY).is_none() {
        return Ok(false);
    }
    doc.remove(TOKEN_KEY);
    doc.remove(USER_KEY);
    let text = doc.to_string();
    if text.trim().is_empty() {
        std::fs::remove_file(path).with_context(|| format!("could not remove {}", tilde(path)))?;
    } else {
        write_private(path, &text).with_context(|| format!("could not write {}", tilde(path)))?;
    }
    Ok(true)
}

/// The config file as an editable document, or `None` when there is no
/// file yet.
fn read_document(path: &Path) -> anyhow::Result<Option<DocumentMut>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", tilde(path))),
    };
    let doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("could not parse {}", tilde(path)))?;
    Ok(Some(doc))
}

/// A top-level string key, trimmed, when it is there and not blank.
fn string_key(doc: &DocumentMut, key: &str) -> Option<String> {
    doc.get(key)
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Set a top-level string key, keeping the spacing and comments around an
/// existing one.
fn set_string(doc: &mut DocumentMut, key: &str, text: &str) {
    match doc.get_mut(key).and_then(Item::as_value_mut) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = toml_edit::Value::from(text);
            *existing.decor_mut() = decor;
        }
        None => doc[key] = toml_edit::value(text),
    }
}

/// Write the file so that only the owner can read it: created with mode
/// 600, and set to 600 when it already existed with something wider.
fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write as _;
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

    #[test]
    fn labels_are_short_and_displays_are_prose() {
        assert_eq!(TokenSource::Env(TOKEN_ENV).label(), "ASANA_TOKEN");
        assert_eq!(
            TokenSource::Env(TOKEN_ENV).to_string(),
            "ASANA_TOKEN environment variable"
        );
        assert_eq!(WorkspaceSource::Flag.label(), "--workspace");
        assert_eq!(WorkspaceSource::Flag.to_string(), "--workspace flag");
        assert_eq!(WorkspaceSource::Env.label(), "ASANA_WORKSPACE");
        let file = PathBuf::from("/nonexistent/asana/config.toml");
        assert_eq!(
            TokenSource::ConfigFile(file.clone()).label(),
            "/nonexistent/asana/config.toml"
        );
        assert_eq!(
            WorkspaceSource::ConfigFile(file).label(),
            "/nonexistent/asana/config.toml"
        );
    }

    /// No token: gh's two parts, the login command first, the variable
    /// second, hand-wrapped under 80 columns.
    #[test]
    fn missing_token_points_at_auth_login_first() {
        let msg = ConfigError::MissingToken("~/.config/asana/config.toml".into()).to_string();
        assert_eq!(
            msg,
            "To get started with the Asana CLI, please run:  asana auth login\n\
             Alternatively, populate the ASANA_TOKEN environment variable with a personal\n\
             access token from https://app.asana.com/0/my-apps."
        );
        for line in msg.lines() {
            assert!(line.chars().count() < 80, "{line}");
        }
    }

    #[test]
    fn host_follows_the_base_url() {
        assert_eq!(HOST, "app.asana.com");
        // The environment is shared across tests, so only the default and
        // the parsing are pinned here.
        if std::env::var_os("ASANA_BASE_URL").is_none() {
            assert_eq!(host(), "app.asana.com");
            assert_eq!(base_url(), None);
        }
        let parsed = url::Url::parse("http://127.0.0.1:4321/api/1.0").unwrap();
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    }

    #[test]
    fn save_creates_a_private_file_and_remove_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asana").join("config.toml");
        assert_eq!(stored_login_at(&path).unwrap(), None);
        save_login_at(&path, "2/1/secret", Some("Dave Div")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "token = \"2/1/secret\"\nuser = \"Dave Div\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode {mode:o}");
        }
        assert_eq!(
            stored_login_at(&path).unwrap(),
            Some(Login {
                token: "2/1/secret".to_string(),
                user: Some("Dave Div".to_string()),
            })
        );
        assert!(remove_token_at(&path).unwrap());
        assert!(!path.exists(), "a file login created is removed with it");
        assert!(!remove_token_at(&path).unwrap());
        assert_eq!(stored_login_at(&path).unwrap(), None);
    }

    #[test]
    fn save_keeps_comments_and_other_keys_and_fixes_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# mine\nworkspace = \"Personal\" # the default\ntoken = \"old\" # stale\nuser = \"Old Name\"\n\n[other]\ntoken = \"not ours\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        save_login_at(&path, "new", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\nworkspace = \"Personal\" # the default\ntoken = \"new\" # stale\n\n[other]\ntoken = \"not ours\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode {mode:o}");
        }
        assert_eq!(
            stored_login_at(&path).unwrap(),
            Some(Login {
                token: "new".to_string(),
                user: None,
            })
        );
        // A new key lands with the root keys, before the tables.
        save_login_at(&path, "newer", Some("Dave \"D\" \\ Div")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# mine\nworkspace = \"Personal\" # the default\ntoken = \"newer\" # stale\nuser = "),
            "{text}"
        );
        assert!(
            text.ends_with("\n\n[other]\ntoken = \"not ours\"\n"),
            "{text}"
        );
        assert_eq!(
            stored_login_at(&path).unwrap().unwrap().user.as_deref(),
            Some("Dave \"D\" \\ Div")
        );
        assert!(remove_token_at(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\nworkspace = \"Personal\" # the default\n\n[other]\ntoken = \"not ours\"\n"
        );
        // The token that is left is the other table's, not a login.
        assert_eq!(stored_login_at(&path).unwrap(), None);
        assert!(!remove_token_at(&path).unwrap());
    }

    #[test]
    fn remove_leaves_a_missing_or_tokenless_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(!remove_token_at(&path).unwrap());
        assert!(!path.exists());
        std::fs::write(&path, "workspace = \"P\"\ntoken = \"  \"\n").unwrap();
        assert!(!remove_token_at(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "workspace = \"P\"\ntoken = \"  \"\n"
        );
    }

    #[test]
    fn malformed_config_is_an_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = \n").unwrap();
        for err in [
            save_login_at(&path, "t", None).unwrap_err(),
            remove_token_at(&path).unwrap_err(),
            stored_login_at(&path).unwrap_err(),
        ] {
            assert!(err.to_string().starts_with("could not parse "), "{err}");
        }
    }

    /// The cause is the `#[source]`, not part of the message, so anyhow's
    /// `{:#}` prints it once.
    #[test]
    fn config_errors_carry_their_cause_once() {
        let err = ConfigError::Unreadable {
            path: PathBuf::from("/x/config.toml"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied"),
        };
        assert_eq!(err.to_string(), "could not read /x/config.toml");
        assert_eq!(
            format!("{:#}", anyhow::Error::new(err)),
            "could not read /x/config.toml: Permission denied"
        );
        let err = ConfigError::Malformed {
            path: PathBuf::from("/x/config.toml"),
            source: toml::from_str::<FileConfig>("token = ").unwrap_err(),
        };
        assert_eq!(err.to_string(), "could not parse /x/config.toml");
        let chain = format!("{:#}", anyhow::Error::new(err));
        assert!(
            chain.starts_with("could not parse /x/config.toml: "),
            "{chain}"
        );
    }
}
