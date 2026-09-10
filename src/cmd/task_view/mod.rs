//! `asana task view`: title, metadata, notes and comments of one task,
//! laid out like `gh issue view`.
//!
//! On a terminal: bold title and `<workspace>#<gid>`, a state line
//! (`Open • who created when • N comments`), `Key: value` metadata, the
//! notes wrapped with a two-space margin, then either the newest comment
//! (with a `Not showing N comments` rule) or, with `--comments`, all of
//! them, and a muted footer with the permalink. Piped: `key:\tvalue` lines,
//! `--`, the raw notes; piped with `--comments`, each comment as its own
//! `key:\tvalue` record, as `gh issue view --comments | cat` prints them.
//! `--json [fields]` prints the raw task (plus `comments` when asked for)
//! and `--jq` filters it, through `render::json`.

use std::io::Write;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::{Story, Task, STORY_FIELDS, SUBTASK_FIELDS, TASK_VIEW_FIELDS};
use crate::cmd::{comment_url, finish, open_in_browser, spinner, task_url};
use crate::ctx::Ctx;
use crate::render::json::{self, JsonArgs};
use crate::render::notes::render_body;
use crate::render::time::{due_phrase, fuzzy_ago, fuzzy_ago_abbr, iso, now, today, DueState};
use crate::render::Palette;
use crate::resolve::parse_task_id;

/// The top-level keys `--json` can select: the task as `TASK_VIEW_FIELDS`
/// returns it, plus `comments` (the comment stories, oldest first), which
/// `--comments` adds and `--json comments` asks for on its own.
const JSON_FIELDS: &[&str] = &[
    "gid",
    "name",
    "notes",
    "html_notes",
    "completed",
    "completed_at",
    "assignee",
    "due_on",
    "due_at",
    "start_on",
    "created_at",
    "modified_at",
    "projects",
    "memberships",
    "tags",
    "parent",
    "num_subtasks",
    "permalink_url",
    "created_by",
    "followers",
    "num_likes",
    "custom_fields",
    "workspace",
    "comments",
];

#[derive(clap::Args, Debug)]
#[command(
    about = "View a task",
    long_about = "Display the title, notes, and other information about a task.\n\n\
        With `--web` flag, open the task in a web browser instead.\n\n\
        With `--comments` flag, show every comment on the task, not only the newest.\n\n\
        For more information about output formatting flags, see `asana help formatting`.",
    after_help = "# View a task by gid or by URL\n\
        $ asana task view 1211234567890123\n\
        $ asana task view https://app.asana.com/0/1200000000000000/1211234567890123\n\
        \n\
        # Show the whole conversation\n\
        $ asana task view 1211234567890123 --comments\n\
        \n\
        # Open the task in the browser\n\
        $ asana task view 1211234567890123 --web\n\
        \n\
        # Print the name and due date as JSON\n\
        $ asana task view 1211234567890123 --json name,due_on"
)]
pub struct Args {
    /// Task gid or URL
    #[arg(value_name = "task")]
    pub task: String,

    /// View task comments
    #[arg(short = 'c', long)]
    pub comments: bool,

    #[command(flatten)]
    pub json: JsonArgs,

    /// Open a task in the browser
    #[arg(short = 'w', long)]
    pub web: bool,
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

/// Who posted a comment, or `someone` when Asana sent no author.
fn author_of(story: &Story) -> &str {
    story
        .created_by
        .as_ref()
        .map(|a| a.display())
        .unwrap_or("someone")
}

/// The comments among a task's stories, oldest first (so the last one is
/// the newest), each kept with the JSON it came from for `--json`.
fn comments_of(stories: Vec<Value>) -> (Vec<Story>, Vec<Value>) {
    let mut pairs: Vec<(Story, Value)> = stories
        .into_iter()
        .filter_map(|raw| {
            let story = serde_json::from_value::<Story>(raw.clone()).ok()?;
            story.is_comment().then_some((story, raw))
        })
        .collect();
    pairs.sort_by_key(|(story, _)| story.created_at);
    pairs.into_iter().unzip()
}

/// Notes or a comment laid out as gh lays an issue body out: the shared
/// `render_body` (wrapped to `width - 4`, a two-space margin, every text
/// line padded to `width - 2`), plus glamour's padding of the blank lines
/// inside the body to that same width, so only the margin lines around the
/// body are truly empty. Empty when there is nothing to show.
fn body_lines(html: Option<&str>, text: Option<&str>, width: usize, ink: &Palette) -> Vec<String> {
    let pad_to = width.saturating_sub(4).max(20) + 2;
    render_body(html, text, width, ink)
        .into_iter()
        .map(|line| {
            if line.is_empty() {
                " ".repeat(pad_to)
            } else {
                line
            }
        })
        .collect()
}

/// Whether `--json` output carries the comments: `--comments` asks for
/// them, and so does naming the field (`--json comments`, or a dotted
/// `comments.text`). Otherwise the stories are not even fetched.
fn wants_json_comments(comments_flag: bool, fields: &[&str]) -> bool {
    comments_flag
        || fields
            .iter()
            .any(|f| f.split('.').next() == Some("comments"))
}

/// Asana's own link to a comment (`permalink_url`, asked for in
/// `STORY_FIELDS`), or one composed from the task's when the API sent none.
fn comment_link(task: &Task, story: &Story) -> String {
    story
        .permalink_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| comment_url(task, &story.gid))
}

/// `Mar 31, 2026`: the due date of a finished task, which is history rather
/// than a deadline, so it always carries its year, the way gh dates
/// anything older than a month.
fn full_date(date: chrono::NaiveDate) -> String {
    date.format("%b %-d, %Y").to_string()
}

/// Only the `--json` flags need checking before the token is read: a lone
/// `--jq` or an unknown field is a usage error, as in gh.
impl crate::cmd::Prevalidate for Args {
    const JSON_FIELDS: &'static [&'static str] = JSON_FIELDS;

    fn prevalidate(&self, _interactive: bool) -> Result<()> {
        self.json.check(Self::JSON_FIELDS)
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    let sp = spinner(ctx);
    let as_json = args.json.is_json();
    let json_comments = as_json && wants_json_comments(args.comments, &args.json.fields());
    let need_stories = !args.web && (!as_json || json_comments);
    let task_path = format!("/tasks/{gid}");
    let stories_path = format!("/tasks/{gid}/stories");
    let task_req = ctx
        .api
        .get::<Value>(&task_path, &[("opt_fields", TASK_VIEW_FIELDS)]);
    let stories_req = async {
        if !need_stories {
            return Ok(Vec::new());
        }
        ctx.api
            .get_all::<Value>(&stories_path, &[("opt_fields", STORY_FIELDS)], None)
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
    let (comments, comments_raw) = comments_of(stories);
    let subtasks: Vec<Task> = if task.num_subtasks.unwrap_or(0) > 0 && !as_json {
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

    if as_json {
        let mut obj = raw;
        if json_comments {
            if let Value::Object(map) = &mut obj {
                map.insert("comments".to_string(), Value::Array(comments_raw));
            }
        }
        return json::print(&obj, &args.json, JSON_FIELDS);
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
    } else if args.comments {
        view.print_raw_comments();
    } else {
        view.print_raw();
    }
    Ok(())
}

struct View<'a> {
    task: &'a Task,
    comments: &'a [Story],
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

    /// `key:\tvalue` lines, `--`, the raw notes, like gh when piped.
    fn print_raw(&self) {
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
        let (n_sub, done) = match task.num_subtasks {
            Some(n) => (n.to_string(), self.subtasks_done().to_string()),
            None => (String::new(), String::new()),
        };
        w!(out, "subtasks:\t{n_sub}");
        w!(out, "subtasks-completed:\t{done}");
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
        // The notes as written, with their own line ending, as gh prints
        // the body (`fmt.Fprintln(out, issue.Body)`).
        w!(out, "{}", task.notes.as_deref().unwrap_or(""));
    }

    /// `--comments` when piped: only the comments, each as `key:\tvalue`
    /// lines, `--`, its text, `--`, the shape of gh's raw comment list.
    fn print_raw_comments(&self) {
        let mut out = std::io::stdout().lock();
        for c in self.comments {
            w!(out, "author:\t{}", author_of(c));
            w!(
                out,
                "created:\t{}",
                c.created_at.map(iso).unwrap_or_default()
            );
            w!(out, "edited:\t{}", c.is_edited.unwrap_or(false));
            w!(out, "--");
            w!(out, "{}", c.text.as_deref().unwrap_or(""));
            w!(out, "--");
        }
    }

    fn print_human(&self, ctx: &Ctx, with_comments: bool) {
        let mut out = std::io::stdout().lock();
        let ink = Palette::new(ctx.color);
        let task = self.task;
        let width = ctx.width;

        // Header: bold title, then `<workspace>#<gid>` in plain text, the
        // way gh 2.97 prints `cli/cli#13111` after the title.
        let ws_name = task
            .workspace
            .as_ref()
            .and_then(|w| w.name.clone())
            .or_else(|| ctx.config.workspace.as_ref().map(|w| w.value.clone()))
            .unwrap_or_else(|| "asana".to_string());
        w!(out, "{} {ws_name}#{}", ink.bold(task.display()), task.gid);

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
            // A finished task is never overdue: its due date is history, so
            // it prints as an absolute date (`Mar 31, 2026`). An open task's
            // reads against today (`today`, `3 days overdue`, `Sep 13`).
            let painted = if task.is_completed() {
                full_date(d)
            } else {
                let (phrase, state) = due_phrase(d, today());
                match state {
                    DueState::Overdue => ink.red(&phrase),
                    DueState::Today => ink.yellow(&phrase),
                    DueState::Upcoming => phrase,
                }
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
        let body = body_lines(
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

        // Comments: the newest one (preview) or all of them, in the shape of
        // gh's comment list: a rule for the hidden ones, then per comment
        // the header, a blank, the body, a blank, its link (never in the
        // preview) and a blank, one more blank after the last, and the
        // `Use --comments` hint when the preview hides any.
        if !self.comments.is_empty() {
            let preview = !with_comments;
            let shown: &[Story] = if preview {
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
                let mut header = ink.bold(author_of(c));
                let when = c
                    .created_at
                    .map(|t| fuzzy_ago_abbr(self.now, t))
                    .unwrap_or_default();
                if !when.is_empty() {
                    header.push_str(&ink.bold(&format!(" • {when}")));
                }
                if c.is_edited.unwrap_or(false) {
                    header.push_str(&ink.bold(" • Edited"));
                }
                if c.is_pinned.unwrap_or(false) {
                    header.push_str(&ink.bold(" • Pinned"));
                }
                if newest {
                    header.push_str(&ink.bold(" • "));
                    header.push_str(&ink.bold_cyan("Newest comment"));
                }
                w!(out, "{header}");
                if let Some(n) = c.num_likes.filter(|n| *n > 0) {
                    w!(out, "{n} 👍");
                }
                w!(out);
                let body = body_lines(c.html_text.as_deref(), c.text.as_deref(), width, &ink);
                if body.is_empty() {
                    w!(out, "  {}", ink.muted("No body provided"));
                } else {
                    for line in body {
                        w!(out, "{line}");
                    }
                }
                w!(out);
                if !preview {
                    // gh mutes the line and its two newlines in one run.
                    let _ = write!(
                        out,
                        "{}",
                        ink.muted(&format!(
                            "View this comment in Asana: {}\n\n",
                            comment_link(task, c)
                        ))
                    );
                }
                if newest {
                    w!(out);
                }
            }
            if preview && hidden > 0 {
                w!(
                    out,
                    "{}",
                    ink.muted("Use --comments to view the full conversation")
                );
            }
        }

        // Footer, muted through its newline like gh's.
        let _ = write!(
            out,
            "{}",
            ink.muted(&format!("View this task in Asana: {}\n", self.url))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn comments_are_the_comment_stories_oldest_first() {
        let stories = vec![
            serde_json::json!({
                "gid": "3", "type": "comment", "text": "third", "is_edited": true,
                "created_at": "2026-09-03T00:00:00.000Z",
                "created_by": {"gid": "2", "name": "Dave"}
            }),
            serde_json::json!({
                "gid": "9", "type": "system", "text": "added to project",
                "created_at": "2026-09-02T00:00:00.000Z"
            }),
            serde_json::json!({
                "gid": "1", "type": "comment", "text": "first",
                "created_at": "2026-09-01T00:00:00.000Z"
            }),
        ];
        let (comments, raw) = comments_of(stories);
        let gids: Vec<&str> = comments.iter().map(|c| c.gid.as_str()).collect();
        assert_eq!(gids, ["1", "3"]);
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[1]["gid"], "3");
        assert_eq!(comments[1].is_edited, Some(true));
        assert_eq!(author_of(&comments[1]), "Dave");
        assert_eq!(author_of(&comments[0]), "someone");
    }

    #[test]
    fn body_pads_blank_lines_inside_like_glamour() {
        let ink = Palette::new(false);
        let text = "Describe the bug\n\nIt breaks.\n";
        let lines = body_lines(None, Some(text), 120, &ink);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(UnicodeWidthStr::width(line.as_str()), 118, "{line:?}");
        }
        assert_eq!(lines[0].trim_end(), "  Describe the bug");
        assert_eq!(lines[1].trim(), "");
        assert_eq!(lines[2].trim_end(), "  It breaks.");
        assert!(body_lines(Some("<body></body>"), Some("  "), 120, &ink).is_empty());
    }

    #[test]
    fn body_keeps_link_urls_and_numbered_steps() {
        let ink = Palette::new(false);
        let html = "<body>Steps:\n1. Run <code>asana auth login</code>\n2. See <a href=\"https://example.com/docs\">the docs</a></body>";
        let lines: Vec<String> = body_lines(Some(html), None, 80, &ink)
            .into_iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        assert_eq!(
            lines,
            [
                "  Steps:",
                "  1. Run `asana auth login`",
                "  2. See the docs https://example.com/docs",
            ]
        );
    }

    #[test]
    fn plurals() {
        assert_eq!(plural(1, "comment"), "1 comment");
        assert_eq!(plural(0, "comment"), "0 comments");
        assert_eq!(plural(3, "comment"), "3 comments");
    }

    /// A finished task's due date is an absolute date with its year, never
    /// `163 days overdue` (last loop's gap).
    #[test]
    fn completed_due_dates_are_absolute() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 3, 31).expect("date");
        assert_eq!(full_date(d), "Mar 31, 2026");
        let d = chrono::NaiveDate::from_ymd_opt(2027, 1, 5).expect("date");
        assert_eq!(full_date(d), "Jan 5, 2027");
    }

    /// The link under a comment is Asana's own permalink; only when the API
    /// sent none is it composed from the task's.
    #[test]
    fn comment_links_prefer_the_story_permalink() {
        let task = Task {
            gid: "34".to_string(),
            permalink_url: Some("https://app.asana.com/1/11/project/12/task/34".to_string()),
            ..Task::default()
        };
        let sent = Story {
            gid: "56".to_string(),
            permalink_url: Some("https://app.asana.com/0/34/34/56".to_string()),
            ..Story::default()
        };
        assert_eq!(
            comment_link(&task, &sent),
            "https://app.asana.com/0/34/34/56"
        );
        for missing in [None, Some(String::new()), Some("  ".to_string())] {
            let story = Story {
                gid: "56".to_string(),
                permalink_url: missing,
                ..Story::default()
            };
            assert_eq!(
                comment_link(&task, &story),
                "https://app.asana.com/1/11/project/12/task/34/comment/56"
            );
        }
    }

    /// `--json` carries the comments only when `--comments` or the field
    /// list asks for them, so a plain `--json` stays the raw task.
    #[test]
    fn json_comments_are_fetched_only_when_asked() {
        assert!(!wants_json_comments(false, &[]));
        assert!(!wants_json_comments(false, &["gid", "name"]));
        assert!(wants_json_comments(true, &[]));
        assert!(wants_json_comments(false, &["gid", "comments"]));
        assert!(wants_json_comments(false, &["comments.text"]));
    }

    /// Every key the raw task can carry is selectable, `comments` included,
    /// and the list has no duplicates (the help prints it sorted).
    #[test]
    fn json_fields_cover_the_requested_opt_fields() {
        let mut seen: Vec<&str> = Vec::new();
        for f in JSON_FIELDS {
            assert!(!seen.contains(f), "duplicate {f}");
            seen.push(f);
        }
        for opt in TASK_VIEW_FIELDS.split(',') {
            let head = opt.split('.').next().expect("field");
            assert!(
                JSON_FIELDS.contains(&head),
                "{head} missing from JSON_FIELDS"
            );
        }
        assert!(JSON_FIELDS.contains(&"gid"));
        assert!(JSON_FIELDS.contains(&"comments"));
        use crate::cmd::Prevalidate as _;
        assert_eq!(Args::JSON_FIELDS, JSON_FIELDS);
    }
}
