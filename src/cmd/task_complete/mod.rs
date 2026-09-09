//! `asana task complete` (alias `close`) and `asana task reopen`.
//!
//! One status line per task on stderr, shaped like gh's
//! `✓ Closed issue cli/cli#13 (The title of the issue)`: a coloured glyph, the
//! verb in the past tense, the identifier, then the title in parentheses. A
//! task already in the requested state gets `! Task ... is already complete`
//! and still exits 0. stdout stays empty unless `--comment` created a story,
//! whose permalink is printed alone so it pipes.

use anyhow::{bail, Result};
use serde_json::json;

use crate::api::{Story, Task};
use crate::cmd::{finish, spinner};
use crate::ctx::Ctx;
use crate::render::Palette;
use crate::resolve::parse_task_id;

/// Enough of the task to label it and know its state before we touch it.
const FIELDS: &str = "name,completed,permalink_url";

#[derive(clap::Args, Debug)]
#[command(
    about = "Mark tasks complete",
    long_about = "Mark one or more tasks complete.\n\n\
        A task that is already complete is reported and left alone, so the command\n\
        is safe to repeat. With `--comment` a closing comment is added to each task\n\
        first and its link is printed.",
    after_help = "# Complete a task\n\
        $ asana task complete 1211234567890123\n\
        \n\
        # Complete a task and leave a closing comment\n\
        $ asana task complete 1211234567890123 --comment \"Shipped in 2.4\"\n\
        \n\
        # Complete several tasks at once\n\
        $ asana task complete 1211234567890123 1211234567890124\n\
        \n\
        # Complete a task by URL\n\
        $ asana task close https://app.asana.com/0/1200000000000000/1211234567890123"
)]
pub struct CompleteArgs {
    /// Task gids or URLs
    #[arg(value_name = "task", required = true, num_args = 1..)]
    pub tasks: Vec<String>,

    /// Leave a closing comment
    #[arg(short = 'c', long, value_name = "text")]
    pub comment: Option<String>,
}

#[derive(clap::Args, Debug)]
#[command(
    about = "Reopen tasks",
    long_about = "Reopen one or more completed tasks.\n\n\
        A task that is already open is reported and left alone, so the command is\n\
        safe to repeat. With `--comment` a comment is added to each task first and\n\
        its link is printed.",
    after_help = "# Reopen a task\n\
        $ asana task reopen 1211234567890123\n\
        \n\
        # Reopen a task and say why\n\
        $ asana task reopen 1211234567890123 --comment \"Regressed in 2.5\"\n\
        \n\
        # Reopen several tasks at once\n\
        $ asana task reopen 1211234567890123 1211234567890124"
)]
pub struct ReopenArgs {
    /// Task gids or URLs
    #[arg(value_name = "task", required = true, num_args = 1..)]
    pub tasks: Vec<String>,

    /// Add a reopening comment
    #[arg(short = 'c', long, value_name = "text")]
    pub comment: Option<String>,
}

/// Nothing to check before the token is read; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for CompleteArgs {}

/// Nothing to check before the token is read; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for ReopenArgs {}

pub async fn run(ctx: &Ctx, args: CompleteArgs) -> Result<()> {
    set_completed(ctx, &args.tasks, args.comment.as_deref(), true).await
}

pub async fn run_reopen(ctx: &Ctx, args: ReopenArgs) -> Result<()> {
    set_completed(ctx, &args.tasks, args.comment.as_deref(), false).await
}

/// The words that change between `complete` and `reopen`.
struct Wording {
    /// `Completed` / `Reopened`, for the success line.
    verb: &'static str,
    /// `complete` / `open`, for the already-in-that-state line.
    state: &'static str,
}

const COMPLETE: Wording = Wording {
    verb: "Completed",
    state: "complete",
};

const REOPEN: Wording = Wording {
    verb: "Reopened",
    state: "open",
};

/// What happened to one task.
enum Outcome {
    /// The state flipped; carries the comment permalink when one was added.
    Changed(Option<String>),
    /// Already in the requested state; nothing was written.
    Already,
}

async fn set_completed(
    ctx: &Ctx,
    tasks: &[String],
    comment: Option<&str>,
    completed: bool,
) -> Result<()> {
    let p = Palette::new(ctx.color);
    let words = if completed { COMPLETE } else { REOPEN };
    if comment.is_some_and(|c| c.trim().is_empty()) {
        bail!("comment cannot be blank");
    }
    // Reject every bad identifier before touching the API, like gh's
    // `invalid issue format: "abc"` (cli.rs prevalidates too, so this only
    // matters for callers that skip it).
    let gids = tasks
        .iter()
        .map(|t| parse_task_id(t))
        .collect::<Result<Vec<_>>>()?;
    // One task at a time, stopping at the first error, the way `gh issue
    // edit` walks several issues.
    for gid in &gids {
        let sp = spinner(ctx);
        let result = update(ctx, gid, comment, completed).await;
        finish(sp);
        let (task, outcome) = result?;
        let label = label(gid, task.name.as_deref());
        match outcome {
            Outcome::Changed(comment_url) => {
                if let Some(url) = comment_url {
                    println!("{url}");
                }
                eprintln!("{}", done_line(&p, words.verb, &label));
            }
            Outcome::Already => eprintln!("{}", already_line(&p, words.state, &label)),
        }
    }
    Ok(())
}

/// Fetch the task, bail out early if it is already in the requested state,
/// otherwise add the comment (if any) and flip `completed`.
async fn update(
    ctx: &Ctx,
    gid: &str,
    comment: Option<&str>,
    completed: bool,
) -> Result<(Task, Outcome)> {
    let task: Task = ctx
        .api
        .get(&format!("/tasks/{gid}"), &[("opt_fields", FIELDS)])
        .await?;
    if task.is_completed() == completed {
        return Ok((task, Outcome::Already));
    }
    let comment_url = match comment {
        Some(text) => {
            let story: Story = ctx
                .api
                .post(&format!("/tasks/{gid}/stories"), json!({ "text": text }))
                .await?;
            Some(comment_url(&task_url(&task), &story.gid))
        }
        None => None,
    };
    let _updated: Task = ctx
        .api
        .put(&format!("/tasks/{gid}"), json!({ "completed": completed }))
        .await?;
    Ok((task, Outcome::Changed(comment_url)))
}

/// `1211234567890123 (Fix the login page)`, the way gh writes
/// `cli/cli#13 (The title of the issue)`; the gid alone for an unnamed task.
fn label(gid: &str, name: Option<&str>) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!("{gid} ({name})"),
        None => gid.to_string(),
    }
}

/// `✓ Completed task 1211234567890123 (Fix the login page)`.
fn done_line(p: &Palette, verb: &str, label: &str) -> String {
    format!("{} {verb} task {label}", p.success_icon())
}

/// `! Task 1211234567890123 (Fix the login page) is already complete`.
fn already_line(p: &Palette, state: &str, label: &str) -> String {
    format!("{} Task {label} is already {state}", p.warning_icon())
}

/// The task's permalink, or a URL Asana redirects to the task when the API
/// did not send one.
fn task_url(task: &Task) -> String {
    task.permalink_url
        .clone()
        .unwrap_or_else(|| format!("https://app.asana.com/0/0/{}/f", task.gid))
}

/// `https://app.asana.com/0/<project>/<task>/<story>/f`: the task permalink
/// with the story gid appended, the way Asana links to a comment. (Copied
/// from `task_comment` so this piece does not depend on that one's API.)
fn comment_url(task_url: &str, story_gid: &str) -> String {
    let base = task_url.trim_end_matches('/').trim_end_matches("/f");
    format!("{base}/{story_gid}/f")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn label_puts_the_gid_first_and_the_title_in_parentheses() {
        assert_eq!(
            label("1211234567890123", Some("Fix the login page")),
            "1211234567890123 (Fix the login page)"
        );
        assert_eq!(
            label("1211234567890123", Some("  padded  ")),
            "1211234567890123 (padded)"
        );
        assert_eq!(label("1211234567890123", Some("")), "1211234567890123");
        assert_eq!(label("1211234567890123", None), "1211234567890123");
    }

    #[test]
    fn status_lines_read_like_gh() {
        let p = Palette::off();
        let label = "1211234567890123 (Fix the login page)";
        assert_eq!(
            done_line(&p, COMPLETE.verb, label),
            "✓ Completed task 1211234567890123 (Fix the login page)"
        );
        assert_eq!(
            done_line(&p, REOPEN.verb, label),
            "✓ Reopened task 1211234567890123 (Fix the login page)"
        );
        assert_eq!(
            already_line(&p, COMPLETE.state, label),
            "! Task 1211234567890123 (Fix the login page) is already complete"
        );
        assert_eq!(
            already_line(&p, REOPEN.state, label),
            "! Task 1211234567890123 (Fix the login page) is already open"
        );
    }

    #[test]
    fn only_the_glyph_is_coloured() {
        let p = Palette::new(true);
        let line = done_line(&p, COMPLETE.verb, "1 (x)");
        assert!(
            line.starts_with("\x1b[0;32m✓\x1b[0m Completed task 1 (x)"),
            "{line:?}"
        );
        let line = already_line(&p, COMPLETE.state, "1 (x)");
        assert!(
            line.starts_with("\x1b[0;33m!\x1b[0m Task 1 (x) is already complete"),
            "{line:?}"
        );
    }

    #[test]
    fn builds_comment_urls() {
        assert_eq!(
            comment_url("https://app.asana.com/0/12/34", "56"),
            "https://app.asana.com/0/12/34/56/f"
        );
        assert_eq!(
            comment_url("https://app.asana.com/0/12/34/f", "56"),
            "https://app.asana.com/0/12/34/56/f"
        );
        let task = Task {
            gid: "34".into(),
            ..Task::default()
        };
        assert_eq!(task_url(&task), "https://app.asana.com/0/0/34/f");
    }

    #[derive(Parser)]
    struct Complete {
        #[command(flatten)]
        args: CompleteArgs,
    }

    #[derive(Parser)]
    struct Reopen {
        #[command(flatten)]
        args: ReopenArgs,
    }

    #[test]
    fn parses_many_tasks_and_a_comment() {
        let c = Complete::try_parse_from(["x", "1", "2", "-c", "done"]).unwrap();
        assert_eq!(c.args.tasks, vec!["1", "2"]);
        assert_eq!(c.args.comment.as_deref(), Some("done"));
        let c = Complete::try_parse_from(["x", "--comment", "done", "1"]).unwrap();
        assert_eq!(c.args.tasks, vec!["1"]);
        assert!(Complete::try_parse_from(["x"]).is_err());
        let r = Reopen::try_parse_from(["x", "https://app.asana.com/0/1/2"]).unwrap();
        assert_eq!(r.args.tasks, vec!["https://app.asana.com/0/1/2"]);
        assert!(r.args.comment.is_none());
    }
}
