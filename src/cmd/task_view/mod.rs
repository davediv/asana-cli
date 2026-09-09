//! `asana task view`: title, metadata, notes and comments of one task,
//! laid out like `gh issue view`.
//!
//! On a terminal: bold title and muted identifier, a state line
//! (`Open • who created when • N comments`), `Key: value` metadata, the
//! notes wrapped with a two-space margin, then either the newest comment
//! (with a `Not showing N comments` rule) or, with `--comments`, all of
//! them, and a muted footer with the permalink. Piped: `key:\tvalue` lines,
//! `--`, the raw notes.

mod notes;

use std::io::Write;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::api::{Story, Task, STORY_FIELDS, SUBTASK_FIELDS, TASK_VIEW_FIELDS};
use crate::cmd::{finish, open_in_browser, print_json, spinner};
use crate::ctx::Ctx;
use crate::render::time::{due_phrase, fuzzy_ago, fuzzy_ago_abbr, iso, now, today, DueState};
use crate::render::Palette;
use crate::resolve::parse_task_id;

use self::notes::render_body;

/// The shared `STORY_FIELDS` plus `is_edited`, for the ` • Edited` marker.
fn story_fields() -> String {
    format!("{STORY_FIELDS},is_edited")
}

#[derive(clap::Args, Debug)]
#[command(
    about = "View a task",
    long_about = "Display the title, notes, and other information about a task.\n\n\
        With `--web` flag, open the task in a web browser instead.\n\n\
        With `--comments` flag, show every comment on the task instead of only the newest one.",
    after_help = "$ asana task view 1211234567890123\n\
        $ asana task view https://app.asana.com/0/1200000000000000/1211234567890123 --comments\n\
        $ asana task view 1211234567890123 --json"
)]
pub struct Args {
    /// Task gid or URL
    #[arg(value_name = "task")]
    pub task: String,

    /// View task comments
    #[arg(short = 'c', long)]
    pub comments: bool,

    /// Output the task as JSON
    #[arg(long)]
    pub json: bool,

    /// Open a task in the browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

/// A comment story plus the `is_edited` flag the shared `Story` lacks.
#[derive(Debug, Clone, Deserialize)]
struct Comment {
    #[serde(flatten)]
    story: Story,
    is_edited: Option<bool>,
}

impl Comment {
    fn author(&self) -> &str {
        self.story
            .created_by
            .as_ref()
            .map(|a| a.display())
            .unwrap_or("someone")
    }
}

/// `writeln!` to stdout, ignoring errors so a closed pipe (`| head`) ends
/// the command quietly instead of panicking.
macro_rules! w {
    ($out:expr) => {
        let _ = writeln!($out);
    };
    ($out:expr, $($arg:tt)*) => {
        let _ = writeln!($out, $($arg)*);
    };
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn task_url(task: &Task) -> String {
    task.permalink_url
        .clone()
        .unwrap_or_else(|| format!("https://app.asana.com/0/0/{}/f", task.gid))
}

/// The permalink of one comment: the task's `/0/<project>/<task>` link with
/// the story appended, else the project-less form.
fn comment_url(task: &Task, story_gid: &str) -> String {
    let base = task_url(task);
    let trimmed = base.trim_end_matches("/f").trim_end_matches('/');
    let legacy = trimmed
        .strip_prefix("https://app.asana.com/0/")
        .is_some_and(|rest| {
            let mut parts = rest.split('/');
            let is_num = |s: Option<&str>| {
                s.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            };
            is_num(parts.next()) && is_num(parts.next()) && parts.next().is_none()
        });
    if legacy {
        format!("{trimmed}/{story_gid}/f")
    } else {
        format!("https://app.asana.com/0/0/{}/{story_gid}/f", task.gid)
    }
}

/// Nothing to check before the token is read; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for Args {}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    let sp = spinner(ctx);
    let need_stories = !args.web && (!args.json || args.comments);
    let task_path = format!("/tasks/{gid}");
    let stories_path = format!("/tasks/{gid}/stories");
    let story_fields = story_fields();
    let task_req = ctx
        .api
        .get::<Value>(&task_path, &[("opt_fields", TASK_VIEW_FIELDS)]);
    let stories_req = async {
        if !need_stories {
            return Ok(Vec::new());
        }
        ctx.api
            .get_all::<Value>(&stories_path, &[("opt_fields", &story_fields)], None)
            .await
    };
    let (raw, stories) = tokio::join!(task_req, stories_req);
    let raw = match raw {
        Ok(v) => v,
        Err(e) => {
            finish(sp);
            return Err(e.into());
        }
    };
    let task: Task = match serde_json::from_value(raw.clone()) {
        Ok(t) => t,
        Err(e) => {
            finish(sp);
            return Err(anyhow::anyhow!("could not parse task {gid}: {e}"));
        }
    };
    if args.web {
        finish(sp);
        return open_in_browser(&task_url(&task));
    }
    let stories = match stories {
        Ok(s) => s,
        Err(e) => {
            finish(sp);
            return Err(e.into());
        }
    };
    // Comments only (no system stories), oldest first so the last is newest.
    let mut pairs: Vec<(Comment, Value)> = stories
        .into_iter()
        .filter_map(|story| {
            let c = serde_json::from_value::<Comment>(story.clone()).ok()?;
            c.story.is_comment().then_some((c, story))
        })
        .collect();
    pairs.sort_by_key(|(c, _)| c.story.created_at);
    let (comments, comments_raw): (Vec<Comment>, Vec<Value>) = pairs.into_iter().unzip();
    let subtasks: Vec<Task> = if task.num_subtasks.unwrap_or(0) > 0 && !args.json {
        ctx.api
            .get_all(
                &format!("/tasks/{gid}/subtasks"),
                &[("opt_fields", SUBTASK_FIELDS)],
                Some(50),
            )
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    finish(sp);

    if args.json {
        if args.comments {
            let mut obj = raw;
            if let Value::Object(map) = &mut obj {
                map.insert("comments".to_string(), Value::Array(comments_raw));
            }
            return print_json(&obj);
        }
        return print_json(&raw);
    }

    let view = View {
        task: &task,
        comments: &comments,
        subtasks: &subtasks,
        url: task_url(&task),
        now: now(),
    };
    if ctx.is_tty {
        view.print_human(ctx, args.comments);
    } else {
        view.print_raw(args.comments);
    }
    Ok(())
}

struct View<'a> {
    task: &'a Task,
    comments: &'a [Comment],
    subtasks: &'a [Task],
    url: String,
    now: DateTime<Utc>,
}

impl View<'_> {
    fn author(&self) -> String {
        self.task
            .created_by
            .as_ref()
            .map(|c| c.display().to_string())
            .unwrap_or_default()
    }

    fn assignee(&self) -> String {
        self.task
            .assignee
            .as_ref()
            .map(|a| a.display().to_string())
            .unwrap_or_default()
    }

    fn tags(&self) -> String {
        self.task
            .tags
            .iter()
            .map(|t| t.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn due_date(&self) -> Option<chrono::NaiveDate> {
        self.task.due_on.or_else(|| {
            self.task
                .due_at
                .map(|t| t.with_timezone(&chrono::Local).date_naive())
        })
    }

    fn subtasks_done(&self) -> usize {
        self.subtasks.iter().filter(|s| s.is_completed()).count()
    }

    /// `key:\tvalue` lines, `--`, the raw notes (and, with `--comments`,
    /// each comment as its own `--`-delimited record), like gh when piped.
    fn print_raw(&self, with_comments: bool) {
        let mut out = std::io::stdout().lock();
        let task = self.task;
        let state = if task.is_completed() {
            "COMPLETED"
        } else {
            "OPEN"
        };
        w!(out, "title:\t{}", task.display());
        w!(out, "state:\t{state}");
        w!(out, "author:\t{}", self.author());
        w!(out, "tags:\t{}", self.tags());
        w!(out, "comments:\t{}", self.comments.len());
        w!(out, "assignee:\t{}", self.assignee());
        w!(out, "projects:\t{}", task.project_labels().join(", "));
        w!(
            out,
            "due:\t{}",
            self.due_date().map(|d| d.to_string()).unwrap_or_default()
        );
        w!(
            out,
            "parent:\t{}",
            task.parent.as_ref().map(|p| p.display()).unwrap_or("")
        );
        let n_sub = task.num_subtasks.unwrap_or(0);
        w!(out, "subtasks:\t{n_sub}");
        w!(
            out,
            "subtasks-completed:\t{}",
            if n_sub > 0 {
                self.subtasks_done().to_string()
            } else {
                String::new()
            }
        );
        w!(out, "likes:\t{}", task.num_likes.unwrap_or(0));
        w!(
            out,
            "created:\t{}",
            task.created_at.map(iso).unwrap_or_default()
        );
        w!(
            out,
            "modified:\t{}",
            task.modified_at.map(iso).unwrap_or_default()
        );
        w!(out, "gid:\t{}", task.gid);
        w!(out, "url:\t{}", self.url);
        w!(out, "--");
        w!(out, "{}", task.notes.as_deref().unwrap_or("").trim_end());
        if with_comments {
            w!(out, "--");
            for c in self.comments {
                w!(out, "author:\t{}", c.author());
                w!(
                    out,
                    "created:\t{}",
                    c.story.created_at.map(iso).unwrap_or_default()
                );
                w!(out, "edited:\t{}", c.is_edited.unwrap_or(false));
                w!(out, "--");
                w!(out, "{}", c.story.text.as_deref().unwrap_or("").trim_end());
                w!(out, "--");
            }
        }
    }

    fn print_human(&self, ctx: &Ctx, with_comments: bool) {
        let mut out = std::io::stdout().lock();
        let ink = Palette::new(ctx.color);
        let task = self.task;
        let width = ctx.width;

        // Header: bold title, muted `<workspace>#<gid>`.
        let ws_name = task
            .workspace
            .as_ref()
            .and_then(|w| w.name.clone())
            .or_else(|| ctx.config.workspace.as_ref().map(|w| w.value.clone()))
            .unwrap_or_else(|| "asana".to_string());
        w!(
            out,
            "{} {}",
            ink.bold(task.display()),
            ink.muted(&format!("{ws_name}#{}", task.gid))
        );

        // State line.
        let state = if task.is_completed() {
            let when = task
                .completed_at
                .map(|t| format!(" {}", fuzzy_ago(self.now, t)))
                .unwrap_or_default();
            format!("{}{when}", ink.magenta("Completed"))
        } else {
            ink.green("Open")
        };
        let created = task
            .created_at
            .map(|t| fuzzy_ago(self.now, t))
            .unwrap_or_else(|| "some time ago".to_string());
        let author = self.author();
        let opened = if author.is_empty() {
            format!("created {created}")
        } else {
            format!("{author} created {created}")
        };
        w!(
            out,
            "{state} • {opened} • {}",
            plural(self.comments.len(), "comment")
        );

        // Reactions.
        if let Some(n) = task.num_likes.filter(|n| *n > 0) {
            w!(out, "{n} 👍");
        }

        // Metadata.
        let assignee = self.assignee();
        if !assignee.is_empty() {
            w!(out, "{}{assignee}", ink.bold("Assignee: "));
        }
        let tags = self.tags();
        if !tags.is_empty() {
            w!(out, "{}{tags}", ink.bold("Tags: "));
        }
        let projects = task.project_labels().join(", ");
        if !projects.is_empty() {
            w!(out, "{}{projects}", ink.bold("Projects: "));
        }
        if let Some(d) = self.due_date() {
            let (phrase, state) = due_phrase(d, today());
            let painted = match state {
                DueState::Overdue => ink.red(&phrase),
                DueState::Today => ink.yellow(&phrase),
                DueState::Upcoming => phrase,
            };
            w!(out, "{}{painted}", ink.bold("Due: "));
        }
        if let Some(parent) = &task.parent {
            w!(
                out,
                "{}{} {}",
                ink.bold("Parent: "),
                parent.display(),
                ink.muted(&parent.gid)
            );
        }
        if let Some(n) = task.num_subtasks.filter(|n| *n > 0) {
            w!(
                out,
                "{}{} of {n} complete",
                ink.bold("Subtasks: "),
                self.subtasks_done()
            );
            for s in self.subtasks {
                let mark = if s.is_completed() {
                    ink.green("✓")
                } else {
                    ink.muted("○")
                };
                w!(out, "  {mark} {} {}", s.display(), ink.muted(&s.gid));
            }
        }
        for field in &task.custom_fields {
            if let (Some(name), Some(value)) = (
                &field.name,
                field.display_value.as_deref().filter(|v| !v.is_empty()),
            ) {
                w!(out, "{}{value}", ink.bold(&format!("{name}: ")));
            }
        }

        // Body: blank, blank, notes, blank, blank.
        w!(out);
        w!(out);
        let body = render_body(
            task.html_notes.as_deref(),
            task.notes.as_deref(),
            width,
            &ink,
        );
        if body.is_empty() {
            w!(out, "  {}", ink.muted("No description provided"));
        } else {
            for line in body {
                w!(out, "{line}");
            }
        }
        w!(out);
        w!(out);

        // Comments: the newest one (preview) or all of them.
        if !self.comments.is_empty() {
            let preview = !with_comments;
            let shown: &[Comment] = if preview {
                &self.comments[self.comments.len() - 1..]
            } else {
                self.comments
            };
            let hidden = self.comments.len() - shown.len();
            if hidden > 0 {
                w!(
                    out,
                    "{}",
                    ink.muted(&format!(
                        "———————— Not showing {} ————————",
                        plural(hidden, "comment")
                    ))
                );
                w!(out);
                w!(out);
            }
            for (i, c) in shown.iter().enumerate() {
                let newest = i + 1 == shown.len();
                let mut header = ink.bold(c.author());
                let when = c
                    .story
                    .created_at
                    .map(|t| fuzzy_ago_abbr(self.now, t))
                    .unwrap_or_default();
                if !when.is_empty() {
                    header.push_str(&ink.bold(&format!(" • {when}")));
                }
                if c.is_edited.unwrap_or(false) {
                    header.push_str(&ink.bold(" • Edited"));
                }
                if c.story.is_pinned.unwrap_or(false) {
                    header.push_str(&ink.bold(" • Pinned"));
                }
                if newest {
                    header.push_str(&ink.bold(" • "));
                    header.push_str(&ink.bold_cyan("Newest comment"));
                }
                w!(out, "{header}");
                if let Some(n) = c.story.num_likes.filter(|n| *n > 0) {
                    w!(out, "{n} 👍");
                }
                w!(out);
                let body = render_body(
                    c.story.html_text.as_deref(),
                    c.story.text.as_deref(),
                    width,
                    &ink,
                );
                if body.is_empty() {
                    w!(out, "  {}", ink.muted("No body provided"));
                } else {
                    for line in body {
                        w!(out, "{line}");
                    }
                }
                w!(out);
                if !preview {
                    w!(
                        out,
                        "{}",
                        ink.muted(&format!(
                            "View this comment in Asana: {}",
                            comment_url(task, &c.story.gid)
                        ))
                    );
                }
                if !newest {
                    w!(out);
                }
            }
            w!(out);
            if preview {
                if hidden > 0 {
                    w!(
                        out,
                        "{}",
                        ink.muted("Use --comments to view the full conversation")
                    );
                }
            } else {
                w!(out);
            }
        }

        // Footer.
        w!(
            out,
            "{}",
            ink.muted(&format!("View this task in Asana: {}", self.url))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with_url(url: Option<&str>) -> Task {
        Task {
            gid: "1211234567890123".to_string(),
            permalink_url: url.map(str::to_string),
            ..Task::default()
        }
    }

    #[test]
    fn comment_urls_follow_the_task_permalink() {
        let t = task_with_url(Some(
            "https://app.asana.com/0/1200000000000003/1211234567890123",
        ));
        assert_eq!(
            comment_url(&t, "555"),
            "https://app.asana.com/0/1200000000000003/1211234567890123/555/f"
        );
        let t = task_with_url(Some(
            "https://app.asana.com/0/1200000000000003/1211234567890123/f",
        ));
        assert_eq!(
            comment_url(&t, "555"),
            "https://app.asana.com/0/1200000000000003/1211234567890123/555/f"
        );
        let t = task_with_url(Some(
            "https://app.asana.com/1/12/project/34/task/1211234567890123",
        ));
        assert_eq!(
            comment_url(&t, "555"),
            "https://app.asana.com/0/0/1211234567890123/555/f"
        );
        let t = task_with_url(None);
        assert_eq!(task_url(&t), "https://app.asana.com/0/0/1211234567890123/f");
        assert_eq!(
            comment_url(&t, "555"),
            "https://app.asana.com/0/0/1211234567890123/555/f"
        );
    }

    #[test]
    fn comments_deserialize_with_is_edited() {
        let v = serde_json::json!({
            "gid": "1", "type": "comment", "text": "hi", "is_edited": true,
            "created_by": {"gid": "2", "name": "Dave"}
        });
        let c: Comment = serde_json::from_value(v).unwrap();
        assert!(c.story.is_comment());
        assert_eq!(c.is_edited, Some(true));
        assert_eq!(c.author(), "Dave");
        let v = serde_json::json!({"gid": "1", "type": "system", "text": "added to project"});
        let c: Comment = serde_json::from_value(v).unwrap();
        assert!(!c.story.is_comment());
        assert_eq!(c.author(), "someone");
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "comment"), "1 comment");
        assert_eq!(plural(0, "comment"), "0 comments");
        assert_eq!(plural(3, "comment"), "3 comments");
    }
}
