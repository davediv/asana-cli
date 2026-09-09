//! `asana task complete` (alias `close`) and `asana task reopen`.

use anyhow::Result;
use serde_json::json;

use crate::api::Task;
use crate::cmd::{finish, spinner};
use crate::ctx::Ctx;
use crate::render::Palette;
use crate::resolve::parse_task_id;

#[derive(clap::Args, Debug)]
#[command(
    about = "Mark tasks complete",
    long_about = "Mark one or more tasks complete.\n\n\
        Tasks that are already complete are reported and left alone.",
    after_help = "# Complete a task\n\
        $ asana task complete 1211234567890123\n\
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
}

#[derive(clap::Args, Debug)]
#[command(
    about = "Reopen tasks",
    long_about = "Mark one or more completed tasks incomplete again.",
    after_help = "$ asana task reopen 1211234567890123\n$ asana task reopen 1211234567890123 1211234567890124"
)]
pub struct ReopenArgs {
    /// Task gids or URLs
    #[arg(value_name = "task", required = true, num_args = 1..)]
    pub tasks: Vec<String>,
}

pub async fn run(ctx: &Ctx, args: CompleteArgs) -> Result<()> {
    set_completed(ctx, &args.tasks, true).await
}

pub async fn run_reopen(ctx: &Ctx, args: ReopenArgs) -> Result<()> {
    set_completed(ctx, &args.tasks, false).await
}

async fn set_completed(ctx: &Ctx, tasks: &[String], completed: bool) -> Result<()> {
    let p = Palette::new(ctx.color);
    let gids = tasks
        .iter()
        .map(|t| parse_task_id(t))
        .collect::<Result<Vec<_>>>()?;
    for gid in gids {
        let sp = spinner(ctx);
        let task: Result<Task, _> = ctx
            .api
            .get(
                &format!("/tasks/{gid}"),
                &[("opt_fields", "name,completed,permalink_url")],
            )
            .await;
        let task = match task {
            Ok(t) => t,
            Err(e) => {
                finish(sp);
                return Err(e.into());
            }
        };
        let label = format!("{} ({})", task.display(), task.gid);
        if task.is_completed() == completed {
            finish(sp);
            let state = if completed { "complete" } else { "open" };
            eprintln!("{} Task {label} is already {state}", p.warning_icon());
            continue;
        }
        let updated: Result<Task, _> = ctx
            .api
            .put(&format!("/tasks/{gid}"), json!({ "completed": completed }))
            .await;
        finish(sp);
        updated?;
        let verb = if completed { "Completed" } else { "Reopened" };
        eprintln!("{} {verb} task {label}", p.success_icon());
    }
    Ok(())
}
