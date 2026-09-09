//! `asana task assign`: give a task to someone (or to nobody).
//!
//! Modelled on `gh issue edit --add-assignee`: the only thing printed on
//! success is the task's permalink, alone on stdout, so it pipes.

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::ApiError;
use crate::cmd::{finish, spinner};
use crate::ctx::Ctx;
use crate::resolve::{parse_task_id, resolve_assignee};

#[derive(clap::Args, Debug)]
#[command(
    about = "Assign a task to someone",
    long_about = "Assign a task to a person in the workspace, or take it away from them.\n\n\
        A task has at most one assignee, so assigning it replaces whoever had it before.\n\
        The assignee can be given by email address, by user gid, or by name; a name that\n\
        matches more than one person is an error that lists the candidates with their\n\
        emails and gids.\n\n\
        The `<assignee>` argument supports the following special values:\n\
        - `@me`: assign yourself (`me` works too)",
    after_help = "$ asana task assign 1211234567890123 \"@me\"\n\
        $ asana task assign 1211234567890123 jane@example.com\n\
        $ asana task assign 1211234567890123 \"Jane Doe\"\n\
        $ asana task assign https://app.asana.com/0/1200000000000000/1211234567890123 1200000000000001\n\
        $ asana task assign 1211234567890123 --unassign"
)]
pub struct Args {
    /// Task gid or URL
    #[arg(value_name = "task")]
    pub task: String,

    /// "@me", an email, a name or a user gid
    #[arg(
        value_name = "assignee",
        required_unless_present = "unassign",
        conflicts_with = "unassign"
    )]
    pub assignee: Option<String>,

    /// Remove the assignee from the task
    #[arg(long)]
    pub unassign: bool,
}

/// The slice of the updated task we need back from `PUT /tasks/{gid}`.
#[derive(Debug, Clone, Default, Deserialize)]
struct Updated {
    #[serde(default)]
    gid: String,
    permalink_url: Option<String>,
}

/// `me` and `@me` are understood by the API itself, so they need no lookup.
pub fn self_assignee(input: &str) -> bool {
    matches!(input.trim(), "me" | "@me")
}

/// The `assignee` value to send: `null` for `--unassign`, `me`, or a user gid.
async fn assignee_value(ctx: &Ctx, assignee: Option<&str>) -> Result<Value> {
    let Some(input) = assignee else {
        return Ok(Value::Null);
    };
    if self_assignee(input) {
        return Ok(Value::String("me".to_string()));
    }
    match resolve_assignee(ctx, input).await {
        Ok(a) => Ok(Value::String(a.gid)),
        Err(err) => Err(not_found_message(err, input)),
    }
}

/// Keep Asana's own 404 text when it names what was looked up (like
/// `user: Unknown object: x@y.z`); a bare `Not Found` gets the identifier.
fn not_found_message(err: anyhow::Error, input: &str) -> anyhow::Error {
    let wanted = input.trim();
    match err.downcast_ref::<ApiError>() {
        Some(ApiError::NotFound { message, .. })
            if !message.to_lowercase().contains(&wanted.to_lowercase()) =>
        {
            anyhow!("no user matching \"{wanted}\"")
        }
        _ => err,
    }
}

/// The permalink to print; Asana's `/0/0/<gid>/f` form when the API sent none.
pub fn task_url(gid: &str, permalink: Option<&str>) -> String {
    match permalink.filter(|u| !u.trim().is_empty()) {
        Some(u) => u.trim().to_string(),
        None => format!("https://app.asana.com/0/0/{gid}/f"),
    }
}

/// Nothing to check before the token is read; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for Args {}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    if let Some(blank) = args.assignee.as_deref().filter(|a| a.trim().is_empty()) {
        bail!("invalid assignee: \"{blank}\"");
    }
    let sp = spinner(ctx);
    let assignee = match assignee_value(ctx, args.assignee.as_deref()).await {
        Ok(v) => v,
        Err(e) => {
            finish(sp);
            return Err(e);
        }
    };
    let updated: Result<Updated, ApiError> = ctx
        .api
        .put(&format!("/tasks/{gid}"), json!({ "assignee": assignee }))
        .await;
    finish(sp);
    let task = updated?;
    let gid = if task.gid.is_empty() { gid } else { task.gid };
    println!("{}", task_url(&gid, task.permalink_url.as_deref()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        Harness::try_parse_from(std::iter::once("assign").chain(argv.iter().copied()))
            .map(|h| h.args)
    }

    #[test]
    fn me_needs_no_lookup() {
        assert!(self_assignee("me"));
        assert!(self_assignee("@me"));
        assert!(self_assignee(" me "));
        assert!(!self_assignee("mee"));
        assert!(!self_assignee("me@example.com"));
        assert!(!self_assignee(""));
    }

    #[test]
    fn task_urls_prefer_the_permalink() {
        assert_eq!(
            task_url("123", Some("https://app.asana.com/0/9/123")),
            "https://app.asana.com/0/9/123"
        );
        assert_eq!(
            task_url("123", Some("  ")),
            "https://app.asana.com/0/0/123/f"
        );
        assert_eq!(task_url("123", None), "https://app.asana.com/0/0/123/f");
    }

    #[test]
    fn parses_assignee_or_unassign() {
        let a = parse(&["123", "me"]).unwrap();
        assert_eq!(a.task, "123");
        assert_eq!(a.assignee.as_deref(), Some("me"));
        assert!(!a.unassign);

        let a = parse(&["123", "--unassign"]).unwrap();
        assert_eq!(a.assignee, None);
        assert!(a.unassign);

        let a = parse(&["--unassign", "123"]).unwrap();
        assert_eq!(a.task, "123");
        assert!(a.unassign);
    }

    #[test]
    fn a_missing_assignee_is_a_missing_argument() {
        let err = parse(&["123"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
        let err = parse(&[]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn assignee_and_unassign_conflict() {
        let err = parse(&["123", "me", "--unassign"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn keeps_asana_text_that_names_the_user() {
        let err = anyhow::Error::new(ApiError::NotFound {
            message: "user: Unknown object: x@y.z".to_string(),
            url: "u".to_string(),
        });
        // ApiError's Display belongs to the errors piece; only assert that
        // Asana's own text survives untouched.
        let shown = not_found_message(err, "x@y.z").to_string();
        assert!(shown.contains("user: Unknown object: x@y.z"), "{shown}");
        let err = anyhow::Error::new(ApiError::NotFound {
            message: "Not Found".to_string(),
            url: "u".to_string(),
        });
        assert_eq!(
            not_found_message(err, "x@y.z").to_string(),
            "no user matching \"x@y.z\""
        );
        let err = anyhow::Error::new(ApiError::Forbidden {
            message: "Forbidden".to_string(),
            url: "u".to_string(),
        });
        assert!(not_found_message(err, "x@y.z")
            .downcast_ref::<ApiError>()
            .is_some());
    }
}
