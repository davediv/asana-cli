//! `asana task assign`: give a task to someone.

use anyhow::Result;
use serde_json::json;

use crate::api::Task;
use crate::cmd::{finish, spinner};
use crate::ctx::Ctx;
use crate::render::Palette;
use crate::resolve::{parse_task_id, resolve_assignee};

#[derive(clap::Args, Debug)]
#[command(
    about = "Assign a task to someone",
    long_about = "Assign a task to a person in the workspace.\n\n\
        The assignee can be `me` (or `@me`), an email address, a name, or a user gid.\n\
        Names are matched through Asana's typeahead; an ambiguous name is an error\n\
        that lists the candidates.",
    after_help = "$ asana task assign 1211234567890123 me\n\
        $ asana task assign 1211234567890123 jane@example.com\n\
        $ asana task assign 1211234567890123 \"Jane Doe\""
)]
pub struct Args {
    /// Task gid or URL
    #[arg(value_name = "task")]
    pub task: String,

    /// "me", an email, a name or a user gid
    #[arg(value_name = "assignee")]
    pub assignee: String,
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let p = Palette::new(ctx.color);
    let gid = parse_task_id(&args.task)?;
    let sp = spinner(ctx);
    let assignee = match resolve_assignee(ctx, &args.assignee).await {
        Ok(a) => a,
        Err(e) => {
            finish(sp);
            return Err(e);
        }
    };
    let updated: Result<Task, _> = ctx
        .api
        .put(
            &format!("/tasks/{gid}"),
            json!({ "assignee": assignee.gid }),
        )
        .await;
    finish(sp);
    let task = updated?;
    let url = task
        .permalink_url
        .clone()
        .unwrap_or_else(|| format!("https://app.asana.com/0/0/{}/f", task.gid));
    eprintln!(
        "{} Assigned task {} ({}) to {}",
        p.success_icon(),
        task.display(),
        task.gid,
        assignee.name
    );
    println!("{url}");
    Ok(())
}
