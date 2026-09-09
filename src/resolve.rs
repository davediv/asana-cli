//! Turn what the user typed into Asana identifiers.
//!
//! - tasks: a gid or any app.asana.com task URL
//! - projects: a gid, a URL, or a (case-insensitive, prefix-tolerant) name
//! - assignees: `me`/`@me`, an email, a gid, or a name via typeahead
//!
//! Something that cannot be an identifier at all is an [`InvalidIdentifier`]
//! (`invalid task format: "abc"`, gh's wording), a typed error `error.rs`
//! recognises wherever it sits in an anyhow chain.

use std::fmt;

use anyhow::{bail, Result};
use url::Url;

use crate::api::{Named, Project, PROJECT_FIELDS};
use crate::ctx::Ctx;

/// What an identifier was meant to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `Workspace` waits for a workspace-format check
pub enum IdKind {
    Task,
    Project,
    Assignee,
    Workspace,
}

/// Input that cannot be an identifier of `kind`: not a gid, not a URL of
/// the right shape, and (for the kinds that take one) not a name either.
/// Displays as gh does (`invalid task format: "abc"`); `error.rs` adds what
/// would have been accepted and the command's synopsis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidIdentifier {
    pub kind: IdKind,
    /// As typed, untrimmed.
    pub input: String,
}

impl InvalidIdentifier {
    pub fn new(kind: IdKind, input: &str) -> Self {
        Self {
            kind,
            input: input.to_string(),
        }
    }
}

impl fmt::Display for InvalidIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            IdKind::Task => write!(f, "invalid task format: \"{}\"", self.input),
            IdKind::Project => write!(f, "invalid project format: \"{}\"", self.input),
            IdKind::Assignee => write!(f, "invalid assignee: \"{}\"", self.input),
            IdKind::Workspace => write!(f, "invalid workspace format: \"{}\"", self.input),
        }
    }
}

impl std::error::Error for InvalidIdentifier {}

fn is_gid(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

fn path_segments(u: &Url) -> Vec<String> {
    u.path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Extract a task gid from a gid or an Asana URL. Accepted URL shapes:
/// `/0/<project>/<task>`, `/0/<project>/<task>/f`, `/1/<ws>/task/<task>`,
/// `/1/<ws>/project/<p>/task/<task>`, and `.../task/<task>/f`.
pub fn parse_task_id(input: &str) -> Result<String> {
    let s = input.trim();
    if is_gid(s) {
        return Ok(s.to_string());
    }
    let invalid = || anyhow::Error::new(InvalidIdentifier::new(IdKind::Task, input));
    let url = Url::parse(s).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid());
    }
    let segs = path_segments(&url);
    // .../task/<gid>[/f]
    if let Some(pos) = segs.iter().position(|s| s == "task") {
        if let Some(gid) = segs.get(pos + 1).filter(|g| is_gid(g)) {
            return Ok(gid.clone());
        }
        return Err(invalid());
    }
    // /0/<project>/<task>[/f]
    if segs.first().map(String::as_str) == Some("0") {
        if let (Some(project), Some(task)) = (segs.get(1), segs.get(2)) {
            if is_gid(project) && is_gid(task) {
                return Ok(task.clone());
            }
        }
    }
    Err(invalid())
}

/// Extract a project gid from a gid or an Asana URL, if it looks like one.
/// Accepted URL shapes: `/0/<project>/...` (not `/0/0/...`, which is a
/// project-less task link) and `.../project/<project>/...`.
pub fn parse_project_gid(input: &str) -> Option<String> {
    let s = input.trim();
    if is_gid(s) {
        return Some(s.to_string());
    }
    let url = Url::parse(s).ok()?;
    let segs = path_segments(&url);
    if let Some(pos) = segs.iter().position(|s| s == "project") {
        return segs.get(pos + 1).filter(|g| is_gid(g)).cloned();
    }
    if segs.first().map(String::as_str) == Some("0") {
        return segs
            .get(1)
            .filter(|g| is_gid(g) && g.as_str() != "0")
            .cloned();
    }
    None
}

/// Resolve a project by gid, URL or name within the current workspace.
pub async fn resolve_project(ctx: &Ctx, input: &str) -> Result<Project> {
    if let Some(gid) = parse_project_gid(input) {
        let project: Project = ctx
            .api
            .get(
                &format!("/projects/{gid}"),
                &[("opt_fields", PROJECT_FIELDS)],
            )
            .await?;
        return Ok(project);
    }
    let name = input.trim();
    if name.is_empty() {
        return Err(InvalidIdentifier::new(IdKind::Project, input).into());
    }
    let ws = ctx.workspace().await?;
    let projects: Vec<Project> = ctx
        .api
        .get_all(
            "/projects",
            &[
                ("workspace", ws.gid.as_str()),
                ("opt_fields", PROJECT_FIELDS),
            ],
            None,
        )
        .await?;
    let lower = name.to_lowercase();
    let exact: Vec<&Project> = projects
        .iter()
        .filter(|p| p.name.as_deref().is_some_and(|n| n.to_lowercase() == lower))
        .collect();
    let candidates = if exact.is_empty() {
        projects
            .iter()
            .filter(|p| {
                p.name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().starts_with(&lower))
            })
            .collect::<Vec<_>>()
    } else {
        exact
    };
    match candidates.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no project named \"{name}\" in {}", ws.display()),
        many => {
            let mut msg = format!(
                "\"{name}\" matches {} projects in {}:",
                many.len(),
                ws.display()
            );
            for p in many {
                msg.push_str(&format!("\n  {} ({})", p.display(), p.gid));
            }
            msg.push_str("\nUse the gid to pick one.");
            bail!(msg)
        }
    }
}

/// A resolved assignee: the value to send to the API and a display name.
#[derive(Debug, Clone)]
pub struct Assignee {
    /// `me` or a user gid.
    pub gid: String,
    /// The display name, for messages that name the person.
    #[allow(dead_code)]
    pub name: String,
}

/// `me` or `@me`: the special values Asana accepts in place of your own gid.
pub fn is_me(input: &str) -> bool {
    matches!(input.trim(), "me" | "@me")
}

/// The assignee value to send to the API (`me` or a user gid), without
/// asking who `me` is: the fast path for callers that only need the value,
/// like `task assign` and `task create`. Anything else goes through
/// `resolve_assignee`.
#[allow(dead_code)]
pub async fn resolve_assignee_value(ctx: &Ctx, input: &str) -> Result<String> {
    if is_me(input) {
        return Ok("me".to_string());
    }
    Ok(resolve_assignee(ctx, input).await?.gid)
}

/// Resolve `me`/`@me`, an email, a gid, or a name (via typeahead). `me`
/// costs a `GET /users/me` for the display name (cached in `Ctx`, so free
/// when the workspace has already been resolved); use
/// `resolve_assignee_value` when the name is not needed.
pub async fn resolve_assignee(ctx: &Ctx, input: &str) -> Result<Assignee> {
    let s = input.trim();
    if s.is_empty() {
        return Err(InvalidIdentifier::new(IdKind::Assignee, input).into());
    }
    if is_me(s) {
        let me = ctx.me().await?;
        return Ok(Assignee {
            gid: "me".to_string(),
            name: me.display().to_string(),
        });
    }
    if is_gid(s) || s.contains('@') {
        // /users/{gid_or_email}
        let user: Named = ctx
            .api
            .get(&format!("/users/{s}"), &[("opt_fields", "name,email")])
            .await?;
        return Ok(Assignee {
            gid: user.gid.clone(),
            name: user.display().to_string(),
        });
    }
    let ws = ctx.workspace().await?;
    let matches: Vec<Named> = ctx
        .api
        .get_all(
            &format!("/workspaces/{}/typeahead", ws.gid),
            &[
                ("resource_type", "user"),
                ("query", s),
                ("opt_fields", "name,email"),
            ],
            Some(20),
        )
        .await?;
    let lower = s.to_lowercase();
    let exact: Vec<&Named> = matches
        .iter()
        .filter(|u| u.name.as_deref().is_some_and(|n| n.to_lowercase() == lower))
        .collect();
    let candidates: Vec<&Named> = if exact.is_empty() {
        matches.iter().collect()
    } else {
        exact
    };
    match candidates.as_slice() {
        [one] => Ok(Assignee {
            gid: one.gid.clone(),
            name: one.display().to_string(),
        }),
        [] => bail!("no user matching \"{s}\" in {}", ws.display()),
        many => {
            let mut msg = format!("\"{s}\" matches {} people in {}:", many.len(), ws.display());
            for u in many {
                let email = u
                    .email
                    .as_deref()
                    .map(|e| format!(" <{e}>"))
                    .unwrap_or_default();
                msg.push_str(&format!("\n  {}{email} ({})", u.display(), u.gid));
            }
            msg.push_str("\nUse an email or gid to pick one.");
            bail!(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_from_gids_and_urls() {
        assert_eq!(
            parse_task_id("1211234567890123").unwrap(),
            "1211234567890123"
        );
        assert_eq!(
            parse_task_id(" 1211234567890123 ").unwrap(),
            "1211234567890123"
        );
        assert_eq!(
            parse_task_id("https://app.asana.com/0/1200000000000000/1211234567890123").unwrap(),
            "1211234567890123"
        );
        assert_eq!(
            parse_task_id("https://app.asana.com/0/1200000000000000/1211234567890123/f").unwrap(),
            "1211234567890123"
        );
        assert_eq!(
            parse_task_id("https://app.asana.com/0/0/1211234567890123/f").unwrap(),
            "1211234567890123"
        );
        assert_eq!(
            parse_task_id("https://app.asana.com/1/1100000000000000/task/1211234567890123")
                .unwrap(),
            "1211234567890123"
        );
        assert_eq!(
            parse_task_id("https://app.asana.com/1/1100000000000000/project/1200000000000000/task/1211234567890123/f").unwrap(),
            "1211234567890123"
        );
        assert_eq!(
            parse_task_id("https://app.asana.com/1/11/task/1211234567890123?focus=true").unwrap(),
            "1211234567890123"
        );
    }

    #[test]
    fn task_ids_reject_junk() {
        for bad in [
            "abc",
            "",
            "12a",
            "https://app.asana.com/0/123",
            "https://app.asana.com/0/abc/def",
            "ftp://x/task/1",
            "https://app.asana.com/1/11/task/abc",
        ] {
            let err = parse_task_id(bad).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("invalid task format: \"{bad}\""),
                "input {bad:?}"
            );
            assert_eq!(
                err.downcast_ref::<InvalidIdentifier>(),
                Some(&InvalidIdentifier::new(IdKind::Task, bad)),
                "input {bad:?}"
            );
        }
    }

    #[test]
    fn invalid_identifiers_read_like_gh() {
        assert_eq!(
            InvalidIdentifier::new(IdKind::Task, "abc").to_string(),
            "invalid task format: \"abc\""
        );
        assert_eq!(
            InvalidIdentifier::new(IdKind::Project, "").to_string(),
            "invalid project format: \"\""
        );
        assert_eq!(
            InvalidIdentifier::new(IdKind::Assignee, " ").to_string(),
            "invalid assignee: \" \""
        );
        assert_eq!(
            InvalidIdentifier::new(IdKind::Workspace, "?").to_string(),
            "invalid workspace format: \"?\""
        );
    }

    #[test]
    fn me_is_special() {
        assert!(is_me("me"));
        assert!(is_me("@me"));
        assert!(is_me(" me "));
        assert!(!is_me("Me"));
        assert!(!is_me("jane@example.com"));
        assert!(!is_me(""));
    }

    #[test]
    fn project_gids_from_urls() {
        assert_eq!(parse_project_gid("123").as_deref(), Some("123"));
        assert_eq!(
            parse_project_gid("https://app.asana.com/0/1200000000000000/list").as_deref(),
            Some("1200000000000000")
        );
        assert_eq!(
            parse_project_gid("https://app.asana.com/1/11/project/1200000000000000/list")
                .as_deref(),
            Some("1200000000000000")
        );
        assert_eq!(parse_project_gid("Roadmap"), None);
        assert_eq!(
            parse_project_gid("https://app.asana.com/0/0/1211234567890123"),
            None
        );
    }
}
