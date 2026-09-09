//! `asana task comment`: add a comment (story) to a task.

use anyhow::{bail, Result};
use serde_json::json;

use crate::api::{Story, Task};
use crate::cli::usage_error;
use crate::cmd::{can_prompt, finish, prompt_line, read_text_file, spinner};
use crate::ctx::Ctx;
use crate::resolve::parse_task_id;

const PATH: &[&str] = &["task", "comment"];

#[derive(clap::Args, Debug)]
#[command(
    about = "Add a comment to a task",
    long_about = "Add a comment to a task.\n\n\
        Without the body text supplied through flags, the command will interactively\n\
        prompt for the comment text.",
    after_help = "$ asana task comment 1211234567890123 --body \"Hi from the Asana CLI\"\n\
        $ asana task comment 1211234567890123 --body-file notes.txt\n\
        $ echo \"Done\" | asana task comment 1211234567890123 --body-file -"
)]
pub struct Args {
    /// Task gid or URL
    #[arg(value_name = "task")]
    pub task: String,

    /// The comment body text
    #[arg(short = 'b', long, value_name = "text", conflicts_with = "body_file")]
    pub body: Option<String>,

    /// Read body text from file (use "-" to read from standard input)
    #[arg(short = 'F', long, value_name = "file")]
    pub body_file: Option<String>,
}

/// `https://app.asana.com/0/<project>/<task>/<story>/f`: the task permalink
/// with the story gid appended, the way Asana links to a comment.
pub fn comment_url(task_url: &str, story_gid: &str) -> String {
    let base = task_url.trim_end_matches('/').trim_end_matches("/f");
    format!("{base}/{story_gid}/f")
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    let body = match (&args.body, &args.body_file) {
        (Some(b), _) => b.clone(),
        (None, Some(f)) => read_text_file(f)?,
        (None, None) => {
            if !can_prompt(ctx) {
                return Err(usage_error(
                    PATH,
                    "must provide `--body` or `--body-file` when not running interactively",
                ));
            }
            prompt_line("Body")?
        }
    };
    if body.trim().is_empty() {
        bail!("comment body cannot be blank");
    }
    let sp = spinner(ctx);
    let task: Result<Task, _> = ctx
        .api
        .get(
            &format!("/tasks/{gid}"),
            &[("opt_fields", "name,permalink_url")],
        )
        .await;
    let task = match task {
        Ok(t) => t,
        Err(e) => {
            finish(sp);
            return Err(e.into());
        }
    };
    let story: Result<Story, _> = ctx
        .api
        .post(&format!("/tasks/{gid}/stories"), json!({ "text": body }))
        .await;
    finish(sp);
    let story = story?;
    let task_url = task
        .permalink_url
        .unwrap_or_else(|| format!("https://app.asana.com/0/0/{}/f", task.gid));
    println!("{}", comment_url(&task_url, &story.gid));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
