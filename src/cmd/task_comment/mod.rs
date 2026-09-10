//! `asana task comment`: add, edit or delete a comment (story) on a task.
//!
//! Shaped like `gh issue comment`: the body comes from `--body`, `--body-file`
//! (`-` reads standard input) or `--editor`; with none of those on a terminal
//! the comment is drafted in the editor and confirmed before it is sent, and
//! `--web` opens the task in the browser instead. `--edit-last` and
//! `--delete-last` act on the last comment you left on the task, with
//! `--create-if-none` and `--yes` as their non-interactive answers. On success
//! the comment's permalink, as Asana itself reports it, is the only thing on
//! stdout, so it pipes.

use std::io::{BufRead, Write};

use anyhow::{bail, Context as _, Result};
use serde_json::json;

use crate::api::{Story, Task};
use crate::cli::usage_error;
use crate::cmd::{
    can_prompt, confirm_default_yes, edit_in_editor, editor_command, editor_name, read_text_file,
    task_url, with_spinner, Prevalidate,
};
use crate::ctx::Ctx;
use crate::render::text::display_url;
use crate::resolve::parse_task_id;

const PATH: &[&str] = &["task", "comment"];

/// gh's messages when the flags do not add up. They are the whole of
/// `CommentablePreRun`, so they read in gh's voice, backticks and all.
const NEED_FLAGS: &str = "flags required when not running interactively";
const CREATE_IF_NONE_ALONE: &str = "`--create-if-none` can only be used with `--edit-last`";
const YES_ALONE: &str = "`--yes` should only be used with `--delete-last`";
const BODY_WITH_DELETE: &str = "should not provide comment body when using `--delete-last`";
const DELETE_NEEDS_YES: &str = "should provide `--yes` to confirm deletion in non-interactive mode";

/// gh's two errors from the edit and delete paths.
const NO_USER_COMMENTS: &str = "no comments found for current user";
const NOT_CONFIRMED: &str = "deletion not confirmed";

/// The fields a story needs here: who wrote it (so "of the current user"
/// can be answered), what it says (the editor's starting point and the
/// delete prompt's preview) and Asana's own link to it.
const FIELDS: &str = "text,type,created_by.gid,permalink_url";

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

    /// Create a new comment if none are found. Can be used only with --edit-last
    #[arg(long)]
    pub create_if_none: bool,

    /// Delete the last comment of the current user
    #[arg(long, conflicts_with = "edit_last")]
    pub delete_last: bool,

    /// Edit the last comment of the current user
    #[arg(long)]
    pub edit_last: bool,

    /// Skip prompts and open the text editor to write the body in
    #[arg(short = 'e', long, conflicts_with_all = ["body", "body_file"])]
    pub editor: bool,

    /// Open the web browser to write the comment
    #[arg(short = 'w', long, conflicts_with_all = ["body", "body_file", "editor"])]
    pub web: bool,

    /// Skip the delete confirmation prompt when --delete-last is provided
    #[arg(long)]
    pub yes: bool,
}

/// gh's `CommentablePreRun`: the four ways of supplying the body exclude
/// each other (clap says `specify only one of ...` for those), `--yes` and
/// `--create-if-none` only answer a question their own flag asks, and
/// without any body flag the command must be able to prompt. Runs before
/// the token is read, so these exit 1 even when there is no token.
impl Prevalidate for Args {
    fn prevalidate(&self, interactive: bool) -> Result<()> {
        if self.create_if_none && !self.edit_last {
            return Err(usage_error(PATH, CREATE_IF_NONE_ALONE));
        }
        if self.yes && !self.delete_last {
            return Err(usage_error(PATH, YES_ALONE));
        }
        if self.delete_last {
            if self.body_flags() > 0 {
                return Err(usage_error(PATH, BODY_WITH_DELETE));
            }
            if interactive || self.yes {
                return Ok(());
            }
            return Err(usage_error(PATH, DELETE_NEEDS_YES));
        }
        if self.body_flags() == 0 && !interactive {
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
    /// How many of the four body flags were given, gh's `inputFlags`.
    fn body_flags(&self) -> usize {
        [
            self.body.is_some(),
            self.body_file.is_some(),
            self.editor,
            self.web,
        ]
        .into_iter()
        .filter(|given| *given)
        .count()
    }

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

/// The comment's permalink as Asana reports it: the story requests
/// `permalink_url` and the [`Story`] that comes back carries it
/// (`https://app.asana.com/0/<task>/<task>/<story>` today). Only if the
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

/// The comment the delete prompt quotes: the first 40 characters, `...` if
/// there was more, on one line so the question stays one line.
fn preview(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    let mut short: String = flat.chars().take(40).collect();
    if flat.chars().nth(40).is_some() {
        short.push_str("...");
    }
    short
}

/// What `--web` says before it opens the browser, or nothing at all when
/// stdout is not a terminal: a piped or redirected run is not being read,
/// so the line would only land in the file.
fn opening_line(is_tty: bool, url: &str) -> Option<String> {
    is_tty.then(|| format!("Opening {} in your browser.", display_url(url)))
}

/// Say where the browser is going, on stderr, then open it.
fn open_in_browser(ctx: &Ctx, url: &str) -> Result<()> {
    if let Some(line) = opening_line(ctx.is_tty, url) {
        eprintln!("{line}");
    }
    open::that_detached(url).with_context(|| format!("could not open {url}"))?;
    Ok(())
}

/// gh's interactive flow: `- Press Enter to draft your comment in vim... `,
/// wait for Enter, then the editor on `initial`.
fn draft_in_editor(ctx: &Ctx, initial: &str) -> Result<String> {
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
    edit_in_editor(initial)
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

/// Every comment on the task written by the authenticated user, oldest
/// first: Asana's stories are the task's whole history, so system stories
/// and other people's comments are dropped here.
async fn my_comments(ctx: &Ctx, gid: &str) -> Result<Vec<Story>> {
    let me = ctx.me().await?;
    let stories: Vec<Story> = with_spinner(
        ctx,
        "",
        ctx.api.get_all(
            &format!("/tasks/{gid}/stories"),
            &[("opt_fields", FIELDS)],
            None,
        ),
    )
    .await?;
    Ok(stories
        .into_iter()
        .filter(|s| s.is_comment() && s.created_by.as_ref().is_some_and(|u| u.gid == me.gid))
        .collect())
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

/// Replace a comment's text and print its permalink, the way posting one
/// does: the link is the comment's, so both flows pipe the same thing.
async fn put_comment(ctx: &Ctx, task_gid: &str, story_gid: &str, body: &str) -> Result<()> {
    let story: Story = with_spinner(
        ctx,
        "",
        ctx.api.put(
            &format!("/stories/{story_gid}"),
            &[("opt_fields", "permalink_url")],
            json!({ "text": body }),
        ),
    )
    .await?;
    println!("{}", comment_link(task_gid, &story));
    Ok(())
}

/// A new comment: the body from wherever it comes, then one POST. `known`
/// is the task when a caller has already fetched it; without one every path
/// but an inline body looks the task up first, so an unknown identifier
/// fails before the browser opens or the user writes anything.
async fn create(ctx: &Ctx, gid: &str, input: Input, known: Option<&Task>) -> Result<()> {
    match input {
        Input::Inline(text) => {
            let body = require_body(&text)?;
            post_comment(ctx, gid, &body).await
        }
        Input::Web => {
            let url = match known {
                Some(task) => task_url(task),
                None => task_url(&fetch_task(ctx, gid).await?),
            };
            open_in_browser(ctx, &url)
        }
        Input::Editor => {
            if known.is_none() {
                fetch_task(ctx, gid).await?;
            }
            let body = require_body(&edit_in_editor("")?)?;
            post_comment(ctx, gid, &body).await
        }
        Input::Interactive => {
            if known.is_none() {
                fetch_task(ctx, gid).await?;
            }
            let body = require_body(&draft_in_editor(ctx, "")?)?;
            // gh's `Submit?` confirmation: Enter or `y` sends the comment,
            // anything else is gh's `Discarding...` (an error, exit 1).
            if !confirm_default_yes("Submit?")? {
                bail!("Discarding...");
            }
            post_comment(ctx, gid, &body).await
        }
    }
}

/// `--edit-last`: your last comment on the task, rewritten. With nothing of
/// yours there, `--create-if-none` (or, on a terminal, saying yes) writes a
/// new one instead; without either that is gh's `no comments found for
/// current user`.
async fn edit_last(ctx: &Ctx, gid: &str, args: &Args, input: Input) -> Result<()> {
    // The task is fetched before its stories so an unknown identifier is
    // named as a task, which is what was typed, and not as the parent the
    // stories collection would report missing.
    let task = fetch_task(ctx, gid).await?;
    let mine = my_comments(ctx, gid).await?;
    let Some(last) = mine.last() else {
        let prompting = input == Input::Interactive;
        let create_one = args.create_if_none
            || (prompting && confirm_default_yes("No comments found. Create one?")?);
        if !create_one {
            bail!(NO_USER_COMMENTS);
        }
        if prompting {
            eprintln!("No comments found. Creating a new comment.");
        }
        return create(ctx, gid, input, Some(&task)).await;
    };
    let initial = last.text.as_deref().unwrap_or("");
    let body = match input {
        Input::Inline(text) => require_body(&text)?,
        // The browser opens on the comment itself, not on the task: that is
        // what is being edited.
        Input::Web => return open_in_browser(ctx, &comment_link(gid, last)),
        Input::Editor => require_body(&edit_in_editor(initial)?)?,
        Input::Interactive => {
            let body = require_body(&draft_in_editor(ctx, initial)?)?;
            if !confirm_default_yes("Submit?")? {
                bail!("Discarding...");
            }
            body
        }
    };
    put_comment(ctx, gid, &last.gid, &body).await
}

/// `--delete-last`: your last comment on the task, removed. On a terminal
/// the deletion is confirmed first (`--yes` skips that, and is required off
/// a terminal); the only line printed is gh's `Comment deleted`, on stderr.
async fn delete_last(ctx: &Ctx, gid: &str, args: &Args, interactive: bool) -> Result<()> {
    fetch_task(ctx, gid).await?;
    let mine = my_comments(ctx, gid).await?;
    let Some(last) = mine.last() else {
        bail!(NO_USER_COMMENTS);
    };
    if interactive && !args.yes {
        let palette = ctx.stderr_palette();
        eprintln!(
            "{} Deleted comments cannot be recovered.",
            palette.warning_icon()
        );
        let quoted = preview(last.text.as_deref().unwrap_or(""));
        if !confirm_default_yes(&format!("Delete the comment: {quoted:?}?"))? {
            bail!(NOT_CONFIRMED);
        }
    }
    with_spinner(ctx, "", ctx.api.delete(&format!("/stories/{}", last.gid))).await?;
    eprintln!("Comment deleted");
    Ok(())
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    let interactive = can_prompt(ctx);
    // Deleting takes no body, so it never asks where one would come from.
    if args.delete_last {
        return delete_last(ctx, &gid, &args, interactive).await;
    }
    // Inline text is checked before touching the network so a blank body
    // fails fast and creates nothing; a bad task fails on the request itself
    // with the same message a lookup would give, and creates nothing either.
    let input = args.input(interactive)?;
    if args.edit_last {
        return edit_last(ctx, &gid, &args, input).await;
    }
    create(ctx, &gid, input, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Named;

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

    /// The delete prompt quotes one line of the comment, cut at 40
    /// characters (not bytes, so an emoji cannot split).
    #[test]
    fn the_delete_prompt_quotes_a_short_flat_preview() {
        assert_eq!(preview("short"), "short");
        assert_eq!(preview("two\nlines"), "two lines");
        assert_eq!(preview(&"a".repeat(40)), "a".repeat(40));
        assert_eq!(preview(&"a".repeat(41)), format!("{}...", "a".repeat(40)));
        let wide = "🙂".repeat(41);
        assert_eq!(preview(&wide), format!("{}...", "🙂".repeat(40)));
    }

    /// `--web` announces itself on a terminal and says nothing when stdout
    /// is redirected, where the line would only be in the way. The URL
    /// loses its scheme, as every link we print in a sentence does.
    #[test]
    fn the_browser_line_is_for_terminals_only() {
        assert_eq!(
            opening_line(true, "https://app.asana.com/0/1/2").as_deref(),
            Some("Opening app.asana.com/0/1/2 in your browser.")
        );
        assert_eq!(opening_line(false, "https://app.asana.com/0/1/2"), None);
    }

    /// Only the current user's comments are candidates: system stories and
    /// other people's comments are not.
    #[test]
    fn mine_are_the_comments_i_wrote() {
        let by = |gid: &str, kind: &str, author: &str| Story {
            gid: gid.to_string(),
            story_type: Some(kind.to_string()),
            created_by: Some(Named {
                gid: author.to_string(),
                ..Named::default()
            }),
            ..Story::default()
        };
        let stories = vec![
            by("1", "system", "me"),
            by("2", "comment", "you"),
            by("3", "comment", "me"),
            by("4", "comment", "me"),
        ];
        let mine: Vec<&str> = stories
            .iter()
            .filter(|s| s.is_comment() && s.created_by.as_ref().is_some_and(|u| u.gid == "me"))
            .map(|s| s.gid.as_str())
            .collect();
        assert_eq!(mine, ["3", "4"]);
        assert_eq!(mine.last(), Some(&"4"));
    }

    fn args(body: Option<&str>, body_file: Option<&str>, editor: bool, web: bool) -> Args {
        Args {
            task: "34".to_string(),
            body: body.map(str::to_string),
            body_file: body_file.map(str::to_string),
            create_if_none: false,
            delete_last: false,
            edit_last: false,
            editor,
            web,
            yes: false,
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

    /// `--edit-last` is a body flag's companion, so it needs none of its
    /// own even when nobody can be prompted: the last comment's text is
    /// the editor's starting point.
    #[test]
    fn prevalidate_guards_the_edit_and_delete_flags() {
        let mut edit = args(None, None, false, false);
        edit.edit_last = true;
        edit.create_if_none = true;
        assert!(edit.prevalidate(true).is_ok());

        let mut lone = args(None, None, false, false);
        lone.create_if_none = true;
        assert_eq!(
            usage_message(lone.prevalidate(true).unwrap_err()),
            CREATE_IF_NONE_ALONE
        );

        let mut lone_yes = args(Some("hi"), None, false, false);
        lone_yes.yes = true;
        assert_eq!(
            usage_message(lone_yes.prevalidate(true).unwrap_err()),
            YES_ALONE
        );

        let mut delete = args(None, None, false, false);
        delete.delete_last = true;
        assert!(delete.prevalidate(true).is_ok());
        assert_eq!(
            usage_message(delete.prevalidate(false).unwrap_err()),
            DELETE_NEEDS_YES
        );
        delete.yes = true;
        assert!(delete.prevalidate(false).is_ok());

        for body in [
            args(Some("hi"), None, false, false),
            args(None, Some("f"), false, false),
            args(None, None, true, false),
            args(None, None, false, true),
        ] {
            let mut with_delete = body;
            with_delete.delete_last = true;
            with_delete.yes = true;
            assert_eq!(
                usage_message(with_delete.prevalidate(true).unwrap_err()),
                BODY_WITH_DELETE
            );
        }
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
            vec!["--edit-last", "--delete-last"],
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
            flag("create-if-none"),
            (
                None,
                None,
                "Create a new comment if none are found. Can be used only with --edit-last"
                    .to_string()
            )
        );
        assert_eq!(
            flag("delete-last"),
            (
                None,
                None,
                "Delete the last comment of the current user".to_string()
            )
        );
        assert_eq!(
            flag("edit-last"),
            (
                None,
                None,
                "Edit the last comment of the current user".to_string()
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
        assert_eq!(
            flag("yes"),
            (
                None,
                None,
                "Skip the delete confirmation prompt when --delete-last is provided".to_string()
            )
        );
    }
}
