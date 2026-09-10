//! `asana task create`: create a task in a project, under a parent task, or
//! in your My Tasks.
//!
//! Shaped like `gh issue create`: flags for everything, prompts for the title
//! and notes when there is a terminal to ask on and no `--title`, a usage
//! error when there is not, `--editor` to write both in `$EDITOR`,
//! `Creating task in <scope>` on stderr, and the new task's URL alone on
//! stdout so it pipes.

use std::io::{BufRead, Write};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _, Result};
use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde_json::{json, Value};

use crate::api::{Task, TASK_FIELDS};
use crate::cli::{usage_error, Cancelled};
use crate::cmd::{
    can_prompt, open_in_browser, read_text_file, task_url, with_spinner, Prevalidate,
};
use crate::ctx::Ctx;
use crate::render::time::today;
use crate::render::Palette;
use crate::resolve::{parse_task_id, resolve_assignee_value, resolve_project};

const PATH: &[&str] = &["task", "create"];

/// gh's wording for a script that forgot the flag
/// (`reference/gh-errors/create-noninteractive.txt`). gh names `--body`
/// too because it insists on one; an Asana task needs no notes, so only the
/// title is required here.
const NON_INTERACTIVE: &str = "must provide `--title` when not running interactively";

#[derive(clap::Args, Debug)]
#[command(
    about = "Create a new task",
    long_about = "Create a task in Asana.\n\n\
        The task goes into the project named by `--project` (a name, gid or URL), or\n\
        under the task named by `--parent` as a subtask. Without either it is created\n\
        in your workspace and assigned to you, so it shows up in My Tasks.\n\n\
        Without `--title`, the command prompts for a title and notes when it can and\n\
        fails when it cannot, so scripts pass `--title` (notes are optional). With\n\
        `--editor`, the first line of the file is the title and the rest is the notes.\n\n\
        The `--assignee` flag supports the following special values:\n\
        - `@me` or `me`: assign yourself\n\n\
        The `--due` flag accepts `YYYY-MM-DD`, `today`, `tomorrow`, or a weekday name\n\
        for the next such day.",
    after_help = "$ asana task create --title \"I found a bug\" --notes \"Nothing works\"\n\
        $ asana task create --title \"Write release notes\" --project \"Roadmap\"\n\
        $ asana task create --title \"Call the vendor\" --assignee \"@me\" --due tomorrow\n\
        $ asana task create --title \"Review the spec\" --assignee jane@example.com --due friday\n\
        $ asana task create --title \"Draft the outline\" --parent 1211234567890123\n\
        $ asana task create --title \"Spec\" --notes-file spec.md\n\
        $ asana task create --title \"Spec\" --notes-file - < spec.md\n\
        $ asana task create --editor\n\
        $ asana task create --project \"Roadmap\" --web"
)]
pub struct Args {
    #[arg(
        short = 't',
        long,
        value_name = "string",
        help = "Supply a title. Will prompt for one otherwise."
    )]
    pub title: Option<String>,

    /// Supply notes (the task's description)
    #[arg(
        short = 'b',
        long,
        value_name = "string",
        conflicts_with = "notes_file"
    )]
    pub notes: Option<String>,

    /// Read notes text from file (use "-" to read from standard input)
    #[arg(short = 'F', long, value_name = "file")]
    pub notes_file: Option<String>,

    /// Skip prompts and open the text editor to write the title and notes in
    #[arg(short = 'e', long, conflicts_with = "web")]
    pub editor: bool,

    /// Add the task to a project by name, gid or URL
    #[arg(short = 'p', long, value_name = "name")]
    pub project: Option<String>,

    /// Add the new task as a subtask of the specified parent task gid or URL
    #[arg(long, value_name = "task")]
    pub parent: Option<String>,

    #[arg(
        short = 'a',
        long,
        value_name = "user",
        help = "Assign a person by email, name or gid. Use \"@me\" to self-assign."
    )]
    pub assignee: Option<String>,

    /// Set the due date: YYYY-MM-DD, "today", "tomorrow" or a weekday
    #[arg(short = 'd', long, value_name = "date")]
    pub due: Option<String>,

    /// Open the browser to create a task
    #[arg(short = 'w', long)]
    pub web: bool,
}

impl Args {
    /// A non-blank `--title`.
    fn title(&self) -> Option<&str> {
        self.title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }

    /// The notes from `--notes` or `--notes-file`, read now so a missing
    /// file fails before anything touches the network.
    fn notes(&self) -> Result<Option<String>> {
        Ok(match (&self.notes, &self.notes_file) {
            (Some(n), _) => Some(n.clone()),
            (None, Some(f)) => Some(read_text_file(f)?),
            (None, None) => None,
        })
    }
}

impl Prevalidate for Args {
    /// Without a terminal to ask on, the title must come from `--title`
    /// (`--web` and `--editor` need none). Runs before the token is read, so
    /// this exits 1 with gh's usage error rather than 4 for a missing token.
    /// A `--parent` that cannot be a task fails here too, like gh's
    /// `invalid issue format`.
    fn prevalidate(&self, interactive: bool) -> Result<()> {
        if let Some(parent) = &self.parent {
            parse_task_id(parent)?;
        }
        if self.web || self.editor || interactive || self.title().is_some() {
            return Ok(());
        }
        Err(usage_error(PATH, NON_INTERACTIVE))
    }
}

/// `YYYY-MM-DD`, `today`, `tomorrow`, or a weekday name (`friday`, `fri`)
/// meaning the next such day after `today`.
pub fn parse_due(input: &str, today: NaiveDate) -> Result<NaiveDate> {
    let s = input.trim().to_lowercase();
    match s.as_str() {
        "today" => return Ok(today),
        "tomorrow" => return Ok(today + Duration::days(1)),
        _ => {}
    }
    if let Some(day) = weekday(&s) {
        let ahead = (7 + i64::from(day.num_days_from_monday())
            - i64::from(today.weekday().num_days_from_monday()))
            % 7;
        let ahead = if ahead == 0 { 7 } else { ahead };
        return Ok(today + Duration::days(ahead));
    }
    NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| {
        anyhow::anyhow!(
            "invalid due date \"{input}\": expected YYYY-MM-DD, today, tomorrow or a weekday"
        )
    })
}

fn weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

/// Notes as sent to Asana: trailing whitespace (the newline `echo`, files
/// and editors leave behind) dropped; nothing but whitespace is no notes.
fn tidy_notes(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    if trimmed.trim_start().is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// What `--editor` (and the `e` answer at the notes prompt) reads back:
/// the first non-blank line is the title, everything after it the notes.
fn split_title_notes(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    let title = lines
        .by_ref()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string);
    let rest: Vec<&str> = lines.collect();
    (title, tidy_notes(rest.join("\n").trim_start_matches('\n')))
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

/// Open the editor on a scratch file holding `initial` and return what was
/// written.
fn edit_in_editor(initial: &str) -> Result<String> {
    let editor = editor_command();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("asana-task-{}-{stamp}.md", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .and_then(|mut f| f.write_all(initial.as_bytes()))
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

/// The file `--editor` opens: the title (or a blank line to type it on), a
/// blank line, then the notes, with a comment explaining the layout the
/// way gh's editor template does.
fn editor_template(title: Option<&str>, notes: Option<&str>) -> String {
    format!(
        "{}\n\n{}\n\n<!-- The first line is the title and the rest is the notes. -->\n",
        title.unwrap_or(""),
        notes.unwrap_or("")
    )
}

/// Drop the `<!-- ... -->` hint `editor_template` added.
fn strip_hint(text: &str) -> String {
    text.lines()
        .filter(|l| !(l.trim_start().starts_with("<!--") && l.trim_end().ends_with("-->")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `? Title (required) ` on stderr, re-asked while the answer is blank, with
/// gh's `X Title cannot be blank` in between. End of input (Ctrl-D) cancels
/// like Ctrl-C does in gh: exit 2, nothing printed.
fn prompt_title(palette: &Palette) -> Result<String> {
    loop {
        let Some(line) = ask("Title (required)")? else {
            return Err(Cancelled.into());
        };
        let title = line.trim();
        if !title.is_empty() {
            return Ok(title.to_string());
        }
        eprintln!("{} Title cannot be blank", palette.failure_icon());
    }
}

/// `? Notes [(e) to launch vi, enter to skip] `, gh's body prompt in one
/// line: Enter skips, `e` opens the editor, anything else is the notes.
fn prompt_notes() -> Result<Option<String>> {
    let editor = editor_command();
    let program = editor
        .split_whitespace()
        .next()
        .and_then(|p| p.rsplit('/').next())
        .unwrap_or("editor");
    let Some(line) = ask(&format!("Notes [(e) to launch {program}, enter to skip]"))? else {
        return Err(Cancelled.into());
    };
    let answer = line.trim();
    if answer.is_empty() {
        return Ok(None);
    }
    if answer == "e" {
        return Ok(tidy_notes(&edit_in_editor("")?));
    }
    Ok(tidy_notes(answer))
}

/// One question on stderr, one line from stdin; `None` at end of input.
fn ask(question: &str) -> Result<Option<String>> {
    eprint!("? {question} ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    let read = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("could not read standard input")?;
    if read == 0 {
        eprintln!();
        return Ok(None);
    }
    Ok(Some(line))
}

/// Everything resolved and ready to send: where the task goes (for the
/// `Creating task in ...` line) and the parts of the `POST /tasks` body
/// that need the API to work out.
struct Prepared {
    /// `task in <project>`, `task in <workspace>` or `subtask of <parent>`.
    scope: String,
    data: serde_json::Map<String, Value>,
}

async fn prepare(ctx: &Ctx, args: &Args) -> Result<Prepared> {
    let ws = ctx.workspace().await?;
    let project = match &args.project {
        Some(p) => Some(resolve_project(ctx, p).await?),
        None => None,
    };
    let parent = match &args.parent {
        Some(p) => {
            let gid = parse_task_id(p)?;
            let parent: Task = ctx
                .api
                .get(&format!("/tasks/{gid}"), &[("opt_fields", TASK_FIELDS)])
                .await?;
            Some(parent)
        }
        None => None,
    };
    // A task with neither a home nor an assignee is only reachable through
    // search, so mirror Asana's own quick-add and put it in My Tasks.
    let assignee = match &args.assignee {
        Some(a) => Some(resolve_assignee_value(ctx, a).await?),
        None if project.is_none() && parent.is_none() => Some("me".to_string()),
        None => None,
    };

    let mut data = serde_json::Map::new();
    data.insert("workspace".into(), Value::String(ws.gid.clone()));
    if let Some(a) = assignee {
        data.insert("assignee".into(), Value::String(a));
    }
    if let Some(p) = &project {
        data.insert("projects".into(), json!([p.gid]));
    }
    if let Some(p) = &parent {
        data.insert("parent".into(), Value::String(p.gid.clone()));
    }

    let scope = match (&parent, &project) {
        (Some(parent), _) => format!("subtask of {}", parent.display()),
        (None, Some(p)) => format!("task in {}", p.display()),
        (None, None) => format!("task in {}", ws.display()),
    };
    Ok(Prepared { scope, data })
}

/// Where `--web` goes: the project's list, or your My Tasks.
async fn web_url(ctx: &Ctx, project: Option<&str>) -> Result<String> {
    let ws = ctx.workspace().await?;
    Ok(match project {
        Some(p) => {
            let p = resolve_project(ctx, p).await?;
            p.permalink_url
                .unwrap_or_else(|| format!("https://app.asana.com/0/{}/list", p.gid))
        }
        None => format!("https://app.asana.com/0/home/{}", ws.gid),
    })
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if args.web {
        let url = with_spinner(ctx, "", web_url(ctx, args.project.as_deref())).await?;
        return open_in_browser(&url);
    }

    // Inline text is read before touching the network so a missing file
    // fails fast and creates nothing.
    let mut title = args.title().map(str::to_string);
    let mut notes = args.notes()?.as_deref().and_then(tidy_notes);
    let due = args
        .due
        .as_deref()
        .map(|d| parse_due(d, today()))
        .transpose()?;
    if title.is_none() && !args.editor && !can_prompt(ctx) {
        // Unreachable after `prevalidate`, kept for callers that skip it.
        return Err(usage_error(PATH, NON_INTERACTIVE));
    }

    let Prepared { scope, mut data } = with_spinner(ctx, "", prepare(ctx, &args)).await?;

    // Like gh, say where the task is going only when someone is watching,
    // and before asking them anything.
    if ctx.is_tty {
        eprintln!();
        eprintln!("Creating {scope}");
        eprintln!();
    }

    if args.editor {
        let text = edit_in_editor(&editor_template(title.as_deref(), notes.as_deref()))?;
        let (t, n) = split_title_notes(&strip_hint(&text));
        title = t;
        notes = n;
        if title.is_none() {
            bail!("title cannot be blank");
        }
    }
    let title = match title {
        Some(t) => t,
        None => {
            // Only reached when every stream is a terminal, so stdout's
            // palette is stderr's too.
            let t = prompt_title(&Palette::new(ctx.color))?;
            if notes.is_none() {
                notes = prompt_notes()?;
            }
            t
        }
    };

    data.insert("name".into(), Value::String(title));
    if let Some(n) = notes {
        data.insert("notes".into(), Value::String(n));
    }
    if let Some(d) = due {
        data.insert("due_on".into(), Value::String(d.to_string()));
    }

    let created: Task = with_spinner(ctx, "", ctx.api.post("/tasks", Value::Object(data))).await?;
    println!("{}", task_url(&created));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("date")
    }

    fn args(title: Option<&str>) -> Args {
        Args {
            title: title.map(str::to_string),
            notes: None,
            notes_file: None,
            editor: false,
            project: None,
            parent: None,
            assignee: None,
            due: None,
            web: false,
        }
    }

    #[test]
    fn parses_due_dates() {
        let today = date(2026, 9, 9); // a Wednesday
        assert_eq!(parse_due("today", today).unwrap(), today);
        assert_eq!(parse_due(" Today ", today).unwrap(), today);
        assert_eq!(
            parse_due("Tomorrow", today).unwrap(),
            today + Duration::days(1)
        );
        assert_eq!(parse_due("2026-12-01", today).unwrap(), date(2026, 12, 1));
    }

    #[test]
    fn weekdays_mean_the_next_one() {
        let wednesday = date(2026, 9, 9);
        assert_eq!(wednesday.weekday(), Weekday::Wed);
        assert_eq!(parse_due("friday", wednesday).unwrap(), date(2026, 9, 11));
        assert_eq!(parse_due("Fri", wednesday).unwrap(), date(2026, 9, 11));
        assert_eq!(parse_due("monday", wednesday).unwrap(), date(2026, 9, 14));
        assert_eq!(parse_due("tue", wednesday).unwrap(), date(2026, 9, 15));
        // The same weekday as today means a week from now, not today.
        assert_eq!(
            parse_due("wednesday", wednesday).unwrap(),
            date(2026, 9, 16)
        );
    }

    #[test]
    fn rejects_other_phrases() {
        let today = date(2026, 9, 9);
        for bad in ["next week", "2026-13-01", "12/01/2026", "", "in 3 days"] {
            let err = parse_due(bad, today).unwrap_err().to_string();
            assert_eq!(
                err,
                format!(
                    "invalid due date \"{bad}\": expected YYYY-MM-DD, today, tomorrow or a weekday"
                ),
                "input {bad:?}"
            );
        }
    }

    /// The message names only `--title`: gh's names `--body` too because gh
    /// insists on one, but notes are optional here.
    #[test]
    fn non_interactive_needs_a_title_before_auth() {
        let err = args(None).prevalidate(false).unwrap_err();
        let usage = err
            .downcast_ref::<crate::cli::UsageError>()
            .expect("a usage error");
        assert_eq!(
            usage.message,
            "must provide `--title` when not running interactively"
        );
        assert_eq!(usage.path, vec!["task", "create"]);
        assert!(args(Some("  ")).prevalidate(false).is_err());
        assert!(args(Some("Fix it")).prevalidate(false).is_ok());
        assert!(args(None).prevalidate(true).is_ok());
        let web = Args {
            web: true,
            ..args(None)
        };
        assert!(web.prevalidate(false).is_ok());
        let editor = Args {
            editor: true,
            ..args(None)
        };
        assert!(editor.prevalidate(false).is_ok());
    }

    #[test]
    fn a_bad_parent_fails_before_auth() {
        let bad = Args {
            parent: Some("abc".to_string()),
            ..args(Some("Fix it"))
        };
        let err = bad.prevalidate(false).unwrap_err();
        assert_eq!(err.to_string(), "invalid task format: \"abc\"");
        assert!(err
            .downcast_ref::<crate::resolve::InvalidIdentifier>()
            .is_some());
        let url = Args {
            parent: Some("https://app.asana.com/0/12/1211234567890123/f".to_string()),
            ..args(Some("Fix it"))
        };
        assert!(url.prevalidate(false).is_ok());
    }

    #[test]
    fn notes_drop_trailing_whitespace_and_blank_is_none() {
        assert_eq!(
            tidy_notes("Nothing works\n").as_deref(),
            Some("Nothing works")
        );
        assert_eq!(
            tidy_notes("  keep the indent\n\n").as_deref(),
            Some("  keep the indent")
        );
        assert_eq!(tidy_notes("   \n\n"), None);
        assert_eq!(tidy_notes(""), None);
    }

    #[test]
    fn editor_text_splits_into_title_and_notes() {
        assert_eq!(
            split_title_notes("I found a bug\n\nNothing works.\nAt all.\n"),
            (
                Some("I found a bug".to_string()),
                Some("Nothing works.\nAt all.".to_string())
            )
        );
        assert_eq!(
            split_title_notes("\n\n  Just a title  \n"),
            (Some("Just a title".to_string()), None)
        );
        assert_eq!(split_title_notes("\n  \n"), (None, None));
        assert_eq!(split_title_notes(""), (None, None));
    }

    #[test]
    fn editor_template_round_trips() {
        let text = editor_template(Some("Title"), Some("Some notes"));
        assert!(text.starts_with("Title\n\nSome notes\n\n<!-- "));
        assert_eq!(
            split_title_notes(&strip_hint(&text)),
            (Some("Title".to_string()), Some("Some notes".to_string()))
        );
        let empty = editor_template(None, None);
        assert_eq!(split_title_notes(&strip_hint(&empty)), (None, None));
    }

    #[test]
    fn help_text_reads_like_gh() {
        use clap::CommandFactory;
        #[derive(clap::Parser)]
        struct Wrap {
            #[command(flatten)]
            args: Args,
        }
        let cmd = Wrap::command();
        let by_name = |n: &str| cmd.get_arguments().find(|a| a.get_long() == Some(n));
        let title = by_name("title").expect("--title");
        assert_eq!(
            title.get_help().map(|h| h.to_string()).as_deref(),
            Some("Supply a title. Will prompt for one otherwise.")
        );
        assert_eq!(title.get_short(), Some('t'));
        let assignee = by_name("assignee").expect("--assignee");
        assert_eq!(
            assignee
                .get_value_names()
                .and_then(|v| v.first())
                .map(|v| v.to_string())
                .as_deref(),
            Some("user")
        );
        assert!(by_name("notes-file").is_some_and(|a| a.get_short() == Some('F')));
        assert!(by_name("editor").is_some_and(|a| a.get_short() == Some('e')));
        assert!(by_name("parent").is_some_and(|a| a.get_short().is_none()));
        assert!(by_name("web").is_some_and(|a| a.get_short() == Some('w')));
    }
}
