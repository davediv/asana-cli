//! `asana task comment`: add a comment (story) to a task.
//!
//! Shaped like `gh issue comment`: the body comes from `--body`, `--body-file`
//! (`-` reads standard input), `--editor`, or an interactive prompt; `--web`
//! opens the task in the browser instead. The task is fetched first so a bad
//! identifier fails with Asana's own message before anything is created, and
//! on success the comment's permalink is the only thing on stdout, so it pipes.

use std::future::Future;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _, Result};
use serde_json::json;
use url::{Position, Url};

use crate::api::{Story, Task};
use crate::cli::usage_error;
use crate::cmd::{can_prompt, finish, open_in_browser, prompt_line, read_text_file, spinner};
use crate::ctx::Ctx;
use crate::resolve::parse_task_id;

const PATH: &[&str] = &["task", "comment"];

#[derive(clap::Args, Debug)]
#[command(
    about = "Add a comment to a task",
    long_about = "Add a comment to an Asana task.\n\n\
        Without the body text supplied through flags, the command will interactively\n\
        prompt for the comment text.",
    after_help = "$ asana task comment 1211234567890123 --body \"Hi from the Asana CLI\"\n\
        $ asana task comment 1211234567890123 --body-file notes.md\n\
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

    /// Skip prompts and open the text editor to write the body in
    #[arg(short = 'e', long, conflicts_with_all = ["body", "body_file"])]
    pub editor: bool,

    /// Open the web browser to write the comment
    #[arg(short = 'w', long, conflicts_with_all = ["body", "body_file", "editor"])]
    pub web: bool,
}

/// Where the comment text comes from, in gh's terms.
#[derive(Debug, PartialEq, Eq)]
enum Input {
    /// `--body` or `--body-file`, already read.
    Inline(String),
    Editor,
    Web,
    Prompt,
}

impl Args {
    fn input(&self, interactive: bool) -> Result<Input> {
        if let Some(text) = &self.body {
            return Ok(Input::Inline(text.clone()));
        }
        if let Some(file) = &self.body_file {
            return Ok(Input::Inline(read_text_file(file)?));
        }
        if self.editor {
            return Ok(Input::Editor);
        }
        if self.web {
            return Ok(Input::Web);
        }
        if !interactive {
            return Err(usage_error(
                PATH,
                "must provide `--body` or `--body-file` when not running interactively",
            ));
        }
        Ok(Input::Prompt)
    }
}

/// The body as sent to Asana: trailing whitespace (the newline `echo` and
/// editors leave behind) dropped; nothing but whitespace is an error.
fn require_body(text: &str) -> Result<String> {
    let body = text.trim_end();
    if body.trim_start().is_empty() {
        bail!("comment body cannot be blank");
    }
    Ok(body.to_string())
}

/// The permalink of a comment, derived from the task's permalink the way
/// Asana's own "Copy comment link" builds it:
///
/// - V0 `https://app.asana.com/0/<project>/<task>[/f]`
///   becomes `https://app.asana.com/0/<project>/<task>/<story>/f`
/// - V1 `https://app.asana.com/1/<workspace>[/project/<p>]/task/<task>[?focus=true]`
///   becomes `https://app.asana.com/1/<workspace>[/project/<p>]/task/<task>/comment/<story>`
///
/// Anything unrecognised falls back to the project-less V0 form.
pub fn comment_url(task_url: &str, task_gid: &str, story_gid: &str) -> String {
    let fallback = || format!("https://app.asana.com/0/0/{task_gid}/{story_gid}/f");
    let Ok(url) = Url::parse(task_url) else {
        return fallback();
    };
    if !matches!(url.scheme(), "http" | "https") {
        return fallback();
    }
    let origin = &url[..Position::BeforePath];
    let segs: Vec<&str> = url
        .path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    match segs.as_slice() {
        ["0", project, task, ..] if is_gid(project) && is_gid(task) => {
            format!("{origin}/0/{project}/{task}/{story_gid}/f")
        }
        ["1", ..] => {
            let task_at = segs
                .iter()
                .position(|s| *s == "task")
                .map(|pos| pos + 1)
                .filter(|i| segs.get(*i).is_some_and(|t| is_gid(t)));
            match task_at {
                Some(i) => format!("{origin}/{}/comment/{story_gid}", segs[..=i].join("/")),
                None => fallback(),
            }
        }
        _ => fallback(),
    }
}

fn is_gid(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The task's permalink, or the project-less V0 form when Asana sent none.
fn task_permalink(task: &Task, gid: &str) -> String {
    task.permalink_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| format!("https://app.asana.com/0/0/{gid}"))
}

/// `$VISUAL`, else `$EDITOR`, else `vi` (`notepad` on Windows), like gh.
fn editor_command() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        })
}

/// Open the editor on a fresh scratch file and return what was written.
fn edit_in_editor() -> Result<String> {
    let editor = editor_command();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path =
        std::env::temp_dir().join(format!("asana-comment-{}-{stamp}.md", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("could not create {}", path.display()))?;
    let status = if cfg!(windows) {
        Command::new("cmd")
            .arg("/C")
            .arg(format!("{editor} \"{}\"", path.display()))
            .status()
    } else {
        Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg(&editor)
            .arg(&path)
            .status()
    };
    let text = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    let status = status.with_context(|| format!("could not run the editor `{editor}`"))?;
    if !status.success() {
        bail!("the editor `{editor}` exited with {status}");
    }
    text.with_context(|| format!("could not read {}", path.display()))
}

/// Run one API call behind the stderr spinner (a no-op when not a terminal).
async fn with_spinner<T, E>(ctx: &Ctx, call: impl Future<Output = Result<T, E>>) -> Result<T, E> {
    let sp = spinner(ctx);
    let out = call.await;
    finish(sp);
    out
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    // Inline text is checked before touching the network so a blank body
    // fails fast and creates nothing.
    let input = match args.input(can_prompt(ctx))? {
        Input::Inline(text) => Input::Inline(require_body(&text)?),
        other => other,
    };

    let task: Task = with_spinner(
        ctx,
        ctx.api.get(
            &format!("/tasks/{gid}"),
            &[("opt_fields", "name,permalink_url")],
        ),
    )
    .await?;
    let task_url = task_permalink(&task, &gid);

    let body = match input {
        Input::Inline(body) => body,
        Input::Web => return open_in_browser(&task_url),
        Input::Editor => require_body(&edit_in_editor()?)?,
        Input::Prompt => require_body(&prompt_line("Body")?)?,
    };

    let story: Story = with_spinner(
        ctx,
        ctx.api
            .post(&format!("/tasks/{gid}/stories"), json!({ "text": body })),
    )
    .await?;
    println!("{}", comment_url(&task_url, &gid, &story.gid));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v0_comment_urls_end_in_story_and_f() {
        assert_eq!(
            comment_url("https://app.asana.com/0/12/34", "34", "56"),
            "https://app.asana.com/0/12/34/56/f"
        );
        assert_eq!(
            comment_url("https://app.asana.com/0/12/34/f", "34", "56"),
            "https://app.asana.com/0/12/34/56/f"
        );
        assert_eq!(
            comment_url("https://app.asana.com/0/0/34", "34", "56"),
            "https://app.asana.com/0/0/34/56/f"
        );
    }

    #[test]
    fn v1_comment_urls_use_the_comment_segment() {
        assert_eq!(
            comment_url("https://app.asana.com/1/11/task/34", "34", "56"),
            "https://app.asana.com/1/11/task/34/comment/56"
        );
        assert_eq!(
            comment_url("https://app.asana.com/1/11/project/12/task/34", "34", "56"),
            "https://app.asana.com/1/11/project/12/task/34/comment/56"
        );
        assert_eq!(
            comment_url(
                "https://app.asana.com/1/11/project/12/task/34?focus=true",
                "34",
                "56"
            ),
            "https://app.asana.com/1/11/project/12/task/34/comment/56"
        );
        assert_eq!(
            comment_url("https://app.asana.com/1/11/task/34/f", "34", "56"),
            "https://app.asana.com/1/11/task/34/comment/56"
        );
    }

    #[test]
    fn unrecognised_permalinks_fall_back_to_v0() {
        for junk in [
            "",
            "not a url",
            "ftp://app.asana.com/0/12/34",
            "https://app.asana.com/1/11/project/12",
            "https://app.asana.com/2/11/task/34",
            "https://app.asana.com/0/abc/34",
        ] {
            assert_eq!(
                comment_url(junk, "34", "56"),
                "https://app.asana.com/0/0/34/56/f",
                "input {junk:?}"
            );
        }
    }

    #[test]
    fn task_permalink_falls_back_when_missing() {
        let mut task = Task {
            gid: "34".to_string(),
            ..Task::default()
        };
        assert_eq!(task_permalink(&task, "34"), "https://app.asana.com/0/0/34");
        task.permalink_url = Some("  ".to_string());
        assert_eq!(task_permalink(&task, "34"), "https://app.asana.com/0/0/34");
        task.permalink_url = Some("https://app.asana.com/1/11/task/34".to_string());
        assert_eq!(
            task_permalink(&task, "34"),
            "https://app.asana.com/1/11/task/34"
        );
    }

    #[test]
    fn bodies_keep_text_and_drop_trailing_newlines() {
        assert_eq!(require_body("Hi from stdin\n").unwrap(), "Hi from stdin");
        assert_eq!(require_body("  indented\r\n\n").unwrap(), "  indented");
        assert_eq!(require_body("two\nlines\n").unwrap(), "two\nlines");
        for blank in ["", " ", "\n\n", "\t \r\n"] {
            assert_eq!(
                require_body(blank).unwrap_err().to_string(),
                "comment body cannot be blank",
                "input {blank:?}"
            );
        }
    }

    fn args(body: Option<&str>, editor: bool, web: bool) -> Args {
        Args {
            task: "34".to_string(),
            body: body.map(str::to_string),
            body_file: None,
            editor,
            web,
        }
    }

    #[test]
    fn input_prefers_flags_then_prompts_only_on_a_terminal() {
        assert_eq!(
            args(Some("hi"), false, false).input(false).unwrap(),
            Input::Inline("hi".to_string())
        );
        assert_eq!(args(None, true, false).input(false).unwrap(), Input::Editor);
        assert_eq!(args(None, false, true).input(false).unwrap(), Input::Web);
        assert_eq!(args(None, false, false).input(true).unwrap(), Input::Prompt);
        let err = args(None, false, false).input(false).unwrap_err();
        let usage = err
            .downcast_ref::<crate::cli::UsageError>()
            .expect("a usage error");
        assert_eq!(
            usage.message,
            "must provide `--body` or `--body-file` when not running interactively"
        );
        assert_eq!(usage.path, vec!["task", "comment"]);
    }
}
