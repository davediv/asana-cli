//! `asana task comment`: add a comment (story) to a task.
//!
//! Shaped like `gh issue comment`: the body comes from `--body`, `--body-file`
//! (`-` reads standard input) or `--editor`; with none of those on a terminal
//! the comment is drafted in the editor and confirmed before it is sent, and
//! `--web` opens the task in the browser instead. On success the comment's
//! permalink, as Asana itself reports it, is the only thing on stdout, so it
//! pipes.

use std::io::{BufRead, Write};

use anyhow::{bail, Context as _, Result};
use serde_json::json;

use crate::api::{Story, Task};
use crate::cli::usage_error;
use crate::cmd::{
    can_prompt, confirm_default_yes, edit_in_editor, editor_command, editor_name, open_in_browser,
    read_text_file, task_url, with_spinner, Prevalidate,
};
use crate::ctx::Ctx;
use crate::resolve::parse_task_id;

const PATH: &[&str] = &["task", "comment"];

/// gh's message when nothing supplies the body and nobody can be asked.
const NEED_FLAGS: &str = "flags required when not running interactively";

#[derive(clap::Args, Debug)]
#[command(
    about = "Add a comment to a task",
    // Ends with a newline like gh's heredoc, so the help screen leaves two
    // blank lines before USAGE the way `gh issue comment --help` does.
    long_about = "Add a comment to an Asana task.\n\n\
        Without the body text supplied through flags, the command will interactively\n\
        prompt for the comment text.\n",
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

/// gh's `CommentablePreRun`: the four ways of supplying the body exclude
/// each other (clap says `specify only one of ...` for those), and without
/// any of them the command must be able to prompt. Runs before the token is
/// read, so this exits 1 even when there is no token.
impl Prevalidate for Args {
    fn prevalidate(&self, interactive: bool) -> Result<()> {
        if !interactive
            && self.body.is_none()
            && self.body_file.is_none()
            && !self.editor
            && !self.web
        {
            return Err(usage_error(PATH, NEED_FLAGS));
        }
        Ok(())
    }
}

/// Where the comment text comes from, in gh's terms.
#[derive(Debug, PartialEq, Eq)]
enum Input {
    /// `--body` or `--body-file`, already read.
    Inline(String),
    /// `--editor`: the editor, no questions asked.
    Editor,
    /// `--web`: the browser, nothing sent from here.
    Web,
    /// No flags on a terminal: draft in the editor, then confirm.
    Interactive,
}

impl Args {
    fn input(&self, interactive: bool) -> Result<Input> {
        if let Some(text) = &self.body {
            return Ok(Input::Inline(text.clone()));
        }
        if let Some(file) = &self.body_file {
            return Ok(Input::Inline(read_text_file(file)?));
        }
        if self.web {
            return Ok(Input::Web);
        }
        if self.editor {
            return Ok(Input::Editor);
        }
        if !interactive {
            return Err(usage_error(PATH, NEED_FLAGS));
        }
        Ok(Input::Interactive)
    }
}

/// The body as sent to Asana: trailing whitespace (the newline `echo` and
/// editors leave behind) dropped; nothing but whitespace is an error.
fn require_body(text: &str) -> Result<String> {
    let body = text.trim_end();
    if body.trim_start().is_empty() {
        bail!("body cannot be blank");
    }
    Ok(body.to_string())
}

/// The comment's permalink as Asana reports it: `POST /tasks/{gid}/stories`
/// is asked for `permalink_url` and the [`Story`] that comes back carries
/// it (`https://app.asana.com/0/<task>/<task>/<story>` today). Only if the
/// field is missing is that same shape built by hand, so what we print is
/// never a guess at Asana's URL scheme when Asana has told us the link.
fn comment_link(task_gid: &str, story: &Story) -> String {
    story
        .permalink_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "https://app.asana.com/0/{task_gid}/{task_gid}/{}",
                story.gid
            )
        })
}

/// gh's interactive flow: `- Press Enter to draft your comment in vim... `,
/// wait for Enter, then the editor.
fn draft_in_editor(ctx: &Ctx) -> Result<String> {
    let palette = ctx.palette();
    let mut out = std::io::stdout();
    write!(
        out,
        "- {} to draft your comment in {}... ",
        palette.bold("Press Enter"),
        palette.bold(&editor_name(&editor_command()))
    )?;
    out.flush()?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("could not read standard input")?;
    edit_in_editor("")
}

/// The task, fetched so a bad identifier fails with Asana's own message
/// before the browser opens or the user writes anything.
async fn fetch_task(ctx: &Ctx, gid: &str) -> Result<Task> {
    let task = with_spinner(
        ctx,
        "",
        ctx.api.get(
            &format!("/tasks/{gid}"),
            &[("opt_fields", "name,permalink_url")],
        ),
    )
    .await?;
    Ok(task)
}

/// Post the comment and print its permalink alone on stdout, like gh.
async fn post_comment(ctx: &Ctx, gid: &str, body: &str) -> Result<()> {
    let story: Story = with_spinner(
        ctx,
        "",
        ctx.api.post(
            &format!("/tasks/{gid}/stories"),
            &[("opt_fields", "permalink_url")],
            json!({ "text": body }),
        ),
    )
    .await?;
    println!("{}", comment_link(gid, &story));
    Ok(())
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    // Inline text is checked before touching the network so a blank body
    // fails fast and creates nothing; a bad task fails on the post itself
    // with the same message a lookup would give, and creates nothing either.
    match args.input(can_prompt(ctx))? {
        Input::Inline(text) => {
            let body = require_body(&text)?;
            post_comment(ctx, &gid, &body).await
        }
        Input::Web => {
            let task = fetch_task(ctx, &gid).await?;
            open_in_browser(&task_url(&task))
        }
        Input::Editor => {
            fetch_task(ctx, &gid).await?;
            let body = require_body(&edit_in_editor("")?)?;
            post_comment(ctx, &gid, &body).await
        }
        Input::Interactive => {
            fetch_task(ctx, &gid).await?;
            let body = require_body(&draft_in_editor(ctx)?)?;
            // gh's `Submit?` confirmation: Enter or `y` sends the comment,
            // anything else is gh's `Discarding...` (an error, exit 1).
            if !confirm_default_yes("Submit?")? {
                bail!("Discarding...");
            }
            post_comment(ctx, &gid, &body).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn story(gid: &str, url: Option<&str>) -> Story {
        Story {
            gid: gid.to_string(),
            permalink_url: url.map(str::to_string),
            ..Story::default()
        }
    }

    #[test]
    fn the_link_is_asanas_own_permalink() {
        let want = "https://app.asana.com/0/34/34/56";
        assert_eq!(comment_link("34", &story("56", Some(want))), want);
        assert_eq!(
            comment_link("34", &story("56", Some("  https://x/y  "))),
            "https://x/y"
        );
    }

    #[test]
    fn a_missing_permalink_is_built_in_the_same_shape() {
        assert_eq!(
            comment_link("34", &story("56", None)),
            "https://app.asana.com/0/34/34/56"
        );
        assert_eq!(
            comment_link("34", &story("56", Some("  "))),
            "https://app.asana.com/0/34/34/56"
        );
    }

    /// What `POST /tasks/{gid}/stories?opt_fields=permalink_url` sends back
    /// decodes into the shared `Story` with or without the link.
    #[test]
    fn the_response_is_read_with_or_without_the_permalink() {
        let with: Story = serde_json::from_str(
            r#"{"gid":"56","resource_type":"story","permalink_url":"https://a/b","text":"hi"}"#,
        )
        .unwrap();
        assert_eq!(with.gid, "56");
        assert_eq!(with.permalink_url.as_deref(), Some("https://a/b"));
        assert_eq!(comment_link("34", &with), "https://a/b");
        let without: Story = serde_json::from_str(r#"{"gid":"56"}"#).unwrap();
        assert_eq!(without.permalink_url, None);
        assert_eq!(
            comment_link("34", &without),
            "https://app.asana.com/0/34/34/56"
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
                "body cannot be blank",
                "input {blank:?}"
            );
        }
    }

    fn args(body: Option<&str>, body_file: Option<&str>, editor: bool, web: bool) -> Args {
        Args {
            task: "34".to_string(),
            body: body.map(str::to_string),
            body_file: body_file.map(str::to_string),
            editor,
            web,
        }
    }

    fn usage_message(err: anyhow::Error) -> String {
        let usage = err
            .downcast_ref::<crate::cli::UsageError>()
            .expect("a usage error");
        assert_eq!(usage.path, vec!["task", "comment"]);
        usage.message.clone()
    }

    #[test]
    fn input_prefers_flags_then_prompts_only_on_a_terminal() {
        let inline = |s: &str| Input::Inline(s.to_string());
        assert_eq!(
            args(Some("hi"), None, false, false).input(false).unwrap(),
            inline("hi")
        );
        assert_eq!(
            args(Some(""), None, false, false).input(false).unwrap(),
            inline("")
        );
        assert_eq!(
            args(None, None, true, false).input(false).unwrap(),
            Input::Editor
        );
        assert_eq!(
            args(None, None, false, true).input(false).unwrap(),
            Input::Web
        );
        assert_eq!(
            args(None, None, false, false).input(true).unwrap(),
            Input::Interactive
        );
        let err = args(None, None, false, false).input(false).unwrap_err();
        assert_eq!(usage_message(err), NEED_FLAGS);
    }

    #[test]
    fn prevalidate_needs_a_flag_only_when_it_cannot_prompt() {
        assert!(args(Some("hi"), None, false, false)
            .prevalidate(false)
            .is_ok());
        assert!(args(Some(""), None, false, false)
            .prevalidate(false)
            .is_ok());
        assert!(args(None, Some("-"), false, false)
            .prevalidate(false)
            .is_ok());
        assert!(args(None, None, true, false).prevalidate(false).is_ok());
        assert!(args(None, None, false, true).prevalidate(false).is_ok());
        assert!(args(None, None, false, false).prevalidate(true).is_ok());

        let err = args(None, None, false, false)
            .prevalidate(false)
            .unwrap_err();
        assert_eq!(usage_message(err), NEED_FLAGS);
    }

    #[test]
    fn the_four_inputs_exclude_each_other_in_clap() {
        use clap::CommandFactory as _;
        let cmd = crate::cli::Cli::command();
        for parts in [
            vec!["--body", "a", "--body-file", "f"],
            vec!["--body", "a", "--editor"],
            vec!["--body-file", "f", "--web"],
            vec!["--editor", "--web"],
        ] {
            let mut argv = vec!["asana", "task", "comment", "1"];
            argv.extend(parts.iter().copied());
            let err = cmd.clone().try_get_matches_from(&argv).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{argv:?}"
            );
        }
    }

    /// The help screen is the one `gh issue comment --help` prints, with
    /// Asana's nouns: the same two paragraphs, two blank lines before
    /// USAGE, and gh's flag descriptions and placeholders word for word.
    #[test]
    fn help_reads_like_gh_issue_comment() {
        use clap::CommandFactory as _;
        let root = crate::cli::Cli::command();
        let cmd = root
            .find_subcommand("task")
            .and_then(|t| t.find_subcommand("comment"))
            .expect("task comment");
        let about = cmd.get_long_about().expect("long about").to_string();
        assert!(about.starts_with("Add a comment to an Asana task.\n\n"));
        assert!(about.ends_with("prompt for the comment text.\n"));
        let flag = |long: &str| {
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_long() == Some(long))
                .unwrap_or_else(|| panic!("--{long}"));
            (
                arg.get_short(),
                arg.get_action()
                    .takes_values()
                    .then(|| arg.get_value_names())
                    .flatten()
                    .map(|v| {
                        v.iter()
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                            .join(" ")
                    }),
                arg.get_help().map(|h| h.to_string()).unwrap_or_default(),
            )
        };
        assert_eq!(
            flag("body"),
            (
                Some('b'),
                Some("text".to_string()),
                "The comment body text".to_string()
            )
        );
        assert_eq!(
            flag("body-file"),
            (
                Some('F'),
                Some("file".to_string()),
                "Read body text from file (use \"-\" to read from standard input)".to_string()
            )
        );
        assert_eq!(
            flag("editor"),
            (
                Some('e'),
                None,
                "Skip prompts and open the text editor to write the body in".to_string()
            )
        );
        assert_eq!(
            flag("web"),
            (
                Some('w'),
                None,
                "Open the web browser to write the comment".to_string()
            )
        );
    }
}
