//! `asana project delete`: delete a project after asking, naming how many
//! tasks go with it. A project takes its tasks with it, so unlike `task
//! delete` nothing is deleted unasked: without a terminal to ask on, `--yes`
//! is required, as gh's `issue delete` requires it.

use anyhow::Result;
use serde::Deserialize;

use crate::api::TASK_COUNT_FIELDS;
use crate::cli::{usage_error, Cancelled};
use crate::cmd::{confirm, finish, spinner};
use crate::ctx::Ctx;
use crate::resolve::resolve_project;

#[derive(clap::Args, Debug)]
#[command(
    about = "Delete a project",
    long_about = "Delete a project. Asana deletes the tasks that are in no other project with it.\n\n\
        A project can be given by gid, by URL, or by name. You are asked to confirm\n\
        first; `--yes` skips the question, and is required when not running\n\
        interactively.",
    after_help = "$ asana project delete Roadmap\n$ asana project delete 1200000000000000 --yes"
)]
pub struct Args {
    /// Project gid, URL or name
    #[arg(value_name = "project")]
    pub project: String,

    /// Confirm deletion without prompting
    #[arg(long)]
    pub yes: bool,
}

/// With nothing to ask on, deleting needs `--yes`; checked before the token
/// is read, see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for Args {
    fn prevalidate(&self, interactive: bool) -> Result<()> {
        if !self.yes && !interactive {
            return Err(usage_error(
                &["project", "delete"],
                "--yes required when not running interactively",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
struct Counts {
    num_tasks: Option<u64>,
}

/// `Delete project Roadmap (1200000000000000) and its 12 tasks?`; no task
/// clause when there are none or the count could not be fetched.
fn question(label: &str, tasks: Option<u64>) -> String {
    match tasks {
        None | Some(0) => format!("Delete project {label}?"),
        Some(1) => format!("Delete project {label} and its 1 task?"),
        Some(n) => format!("Delete project {label} and its {n} tasks?"),
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let sp = spinner(ctx);
    let found = resolve_project(ctx, &args.project).await;
    finish(sp);
    let project = found?;
    let label = format!("{} ({})", project.display(), project.gid);
    if !args.yes {
        let sp = spinner(ctx);
        let counts: Option<Counts> = ctx
            .api
            .get(
                &format!("/projects/{}/task_counts", project.gid),
                &[("opt_fields", TASK_COUNT_FIELDS)],
            )
            .await
            .ok();
        finish(sp);
        if !confirm(&question(&label, counts.and_then(|c| c.num_tasks)))? {
            return Err(Cancelled.into());
        }
    }
    let sp = spinner(ctx);
    let deleted = ctx.api.delete(&format!("/projects/{}", project.gid)).await;
    finish(sp);
    deleted?;
    eprintln!(
        "{} Deleted project {label}",
        ctx.stderr_palette().success_icon()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::Prevalidate;

    #[derive(clap::Parser, Debug)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(argv: &[&str]) -> Args {
        use clap::Parser;
        Cli::try_parse_from(std::iter::once("asana").chain(argv.iter().copied()))
            .expect("parses")
            .args
    }

    #[test]
    fn yes_is_required_only_without_a_terminal() {
        assert!(parse(&["Roadmap"]).prevalidate(true).is_ok());
        assert!(parse(&["Roadmap", "--yes"]).prevalidate(true).is_ok());
        assert!(parse(&["Roadmap", "--yes"]).prevalidate(false).is_ok());
        let err = parse(&["Roadmap"]).prevalidate(false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "--yes required when not running interactively"
        );
    }

    #[test]
    fn the_question_counts_the_tasks() {
        let label = "Roadmap (1200000000000000)";
        assert_eq!(
            question(label, None),
            "Delete project Roadmap (1200000000000000)?"
        );
        assert_eq!(
            question(label, Some(0)),
            "Delete project Roadmap (1200000000000000)?"
        );
        assert_eq!(
            question(label, Some(1)),
            "Delete project Roadmap (1200000000000000) and its 1 task?"
        );
        assert_eq!(
            question(label, Some(12)),
            "Delete project Roadmap (1200000000000000) and its 12 tasks?"
        );
    }
}
