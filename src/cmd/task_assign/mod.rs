//! `asana task assign`: give a task to someone (or to nobody).
//!
//! Modelled on `gh issue edit --add-assignee`: the task's permalink is the
//! only thing printed on stdout, so it pipes, and the confirmation (`✓
//! Assigned task 1211234567890123 (Fix the login page) to Jane Doe`) goes
//! to stderr in the shape of gh's close/reopen lines.

use anyhow::Result;
use serde_json::{json, Value};

use crate::api::Task;
use crate::cmd::{task_url, with_spinner, Prevalidate};
use crate::ctx::Ctx;
use crate::render::Palette;
use crate::resolve::{parse_task_id, resolve_assignee_value, IdKind, InvalidIdentifier};

#[derive(clap::Args, Debug)]
#[command(
    about = "Assign a task to someone",
    long_about = "Assign a task to a person in the workspace, or take it away from them.\n\n\
        A task has at most one assignee, so assigning it replaces whoever had it\n\
        before, and `--unassign` leaves it with nobody. The assignee can be given by\n\
        email address, by user gid, or by name; a name that matches more than one\n\
        person is an error that lists the candidates with their emails and gids.\n\n\
        The `<assignee>` argument supports the following special values:\n\
        - `@me`: assign yourself (`me` works too)",
    after_help = "# Assign a task to yourself\n\
        $ asana task assign 1211234567890123 \"@me\"\n\
        \n\
        # Assign a task to someone else, by email, by name or by user gid\n\
        $ asana task assign 1211234567890123 jane@example.com\n\
        $ asana task assign 1211234567890123 \"Jane Doe\"\n\
        $ asana task assign 1211234567890123 1200000000000001\n\
        \n\
        # Assign a task by URL\n\
        $ asana task assign https://app.asana.com/0/1200000000000000/1211234567890123 \"@me\"\n\
        \n\
        # Take a task away from whoever has it\n\
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

/// A blank assignee can never name anyone, so it fails before the token is
/// read, like gh's `invalid issue format: "abc"` (the task id is checked by
/// `cli.rs` the same way).
impl Prevalidate for Args {
    fn prevalidate(&self, _interactive: bool) -> Result<()> {
        check_assignee(self.assignee.as_deref())
    }
}

/// `invalid assignee: ""` for an assignee that is nothing but whitespace.
fn check_assignee(assignee: Option<&str>) -> Result<()> {
    match assignee {
        Some(blank) if blank.trim().is_empty() => {
            Err(InvalidIdentifier::new(IdKind::Assignee, blank).into())
        }
        _ => Ok(()),
    }
}

/// The `assignee` value to send: `null` for `--unassign`, else `me` or a
/// user gid (an email, gid or name is looked up first).
async fn assignee_value(ctx: &Ctx, assignee: Option<&str>) -> Result<Value> {
    match assignee {
        None => Ok(Value::Null),
        Some(input) => Ok(Value::String(resolve_assignee_value(ctx, input).await?)),
    }
}

/// `1211234567890123 (Fix the login page)`, the way gh writes
/// `cli/cli#13 (The title of the issue)`; the gid alone for an unnamed task.
fn label(task: &Task) -> String {
    match task
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(name) => format!("{} ({name})", task.gid),
        None => task.gid.clone(),
    }
}

/// The stderr confirmation: `✓ Assigned task <label> to <name>` with the
/// name Asana sent back, `✓ Unassigned task <label>` after `--unassign`.
fn status_line(p: &Palette, task: &Task, unassigned: bool) -> String {
    let label = label(task);
    if unassigned {
        return format!("{} Unassigned task {label}", p.success_icon());
    }
    match task
        .assignee
        .as_ref()
        .map(|a| a.display().trim())
        .filter(|n| !n.is_empty())
    {
        Some(name) => format!("{} Assigned task {label} to {name}", p.success_icon()),
        None => format!("{} Assigned task {label}", p.success_icon()),
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    check_assignee(args.assignee.as_deref())?;
    let p = Palette::new(ctx.color);
    let mut task: Task = with_spinner(ctx, "", async {
        let assignee = assignee_value(ctx, args.assignee.as_deref()).await?;
        let task: Task = ctx
            .api
            .put(&format!("/tasks/{gid}"), json!({ "assignee": assignee }))
            .await?;
        Ok::<_, anyhow::Error>(task)
    })
    .await?;
    if task.gid.is_empty() {
        task.gid = gid;
    }
    eprintln!("{}", status_line(&p, &task, args.assignee.is_none()));
    println!("{}", task_url(&task));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Named;
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

    fn task(name: Option<&str>, assignee: Option<&str>) -> Task {
        Task {
            gid: "1211234567890123".to_string(),
            name: name.map(str::to_string),
            assignee: assignee.map(|n| Named {
                gid: "1200000000000001".to_string(),
                name: Some(n.to_string()),
                email: None,
            }),
            ..Task::default()
        }
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

    /// A blank assignee is rejected before any request, with the shared
    /// typed error so `error.rs` prints gh's `invalid assignee: ""`.
    #[test]
    fn blank_assignee_is_an_invalid_identifier() {
        let args = parse(&["123", " "]).unwrap();
        let err = args.prevalidate(true).unwrap_err();
        assert_eq!(err.to_string(), "invalid assignee: \" \"");
        assert_eq!(
            err.downcast_ref::<InvalidIdentifier>(),
            Some(&InvalidIdentifier::new(IdKind::Assignee, " "))
        );
        assert!(parse(&["123", "me"]).unwrap().prevalidate(true).is_ok());
        assert!(parse(&["123", "--unassign"])
            .unwrap()
            .prevalidate(false)
            .is_ok());
    }

    #[test]
    fn labels_read_like_gh() {
        assert_eq!(
            label(&task(Some("Fix the login page"), None)),
            "1211234567890123 (Fix the login page)"
        );
        assert_eq!(label(&task(Some("  "), None)), "1211234567890123");
        assert_eq!(label(&task(None, None)), "1211234567890123");
    }

    #[test]
    fn status_lines_name_who_got_the_task() {
        let p = Palette::off();
        assert_eq!(
            status_line(
                &p,
                &task(Some("Fix the login page"), Some("Jane Doe")),
                false
            ),
            "✓ Assigned task 1211234567890123 (Fix the login page) to Jane Doe"
        );
        assert_eq!(
            status_line(&p, &task(Some("Fix the login page"), None), false),
            "✓ Assigned task 1211234567890123 (Fix the login page)"
        );
        assert_eq!(
            status_line(&p, &task(Some("Fix the login page"), None), true),
            "✓ Unassigned task 1211234567890123 (Fix the login page)"
        );
    }

    #[test]
    fn the_tick_is_green_only_with_colour() {
        let line = status_line(&Palette::new(true), &task(None, Some("Jane Doe")), false);
        assert!(
            line.starts_with("\x1b[0;32m✓\x1b[0m Assigned task"),
            "{line:?}"
        );
        let line = status_line(&Palette::new(false), &task(None, Some("Jane Doe")), false);
        assert!(!line.contains('\x1b'), "{line:?}");
    }
}
