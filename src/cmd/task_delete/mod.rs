//! `asana task delete`: delete a task, asking first on a terminal.

use anyhow::Result;

use crate::api::Task;
use crate::cli::Cancelled;
use crate::cmd::{can_prompt, confirm, finish, spinner};
use crate::ctx::Ctx;
use crate::render::Palette;
use crate::resolve::parse_task_id;

#[derive(clap::Args, Debug)]
#[command(
    about = "Delete a task",
    long_about = "Delete a task. On a terminal you are asked to confirm unless `--yes` is given;\n\
        when not running interactively the task is deleted without a prompt.",
    after_help = "$ asana task delete 1211234567890123\n$ asana task delete 1211234567890123 --yes"
)]
pub struct Args {
    /// Task gid or URL
    #[arg(value_name = "task")]
    pub task: String,

    /// Confirm deletion without prompting
    #[arg(long)]
    pub yes: bool,
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let p = Palette::new(ctx.color);
    let gid = parse_task_id(&args.task)?;
    let sp = spinner(ctx);
    let task: Result<Task, _> = ctx
        .api
        .get(
            &format!("/tasks/{gid}"),
            &[("opt_fields", "name,completed")],
        )
        .await;
    finish(sp);
    let task = task?;
    let label = format!("{} ({})", task.display(), task.gid);
    if !args.yes && can_prompt(ctx) && !confirm(&format!("Delete task {label}?"))? {
        return Err(Cancelled.into());
    }
    let sp = spinner(ctx);
    let deleted = ctx.api.delete(&format!("/tasks/{gid}")).await;
    finish(sp);
    deleted?;
    eprintln!("{} Deleted task {label}", p.success_icon());
    Ok(())
}
