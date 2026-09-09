//! `asana task create`: create a task in a project or in your My Tasks.
//!
//! Shaped like `gh issue create`: flags for everything, a prompt for the
//! title when there is a terminal to ask on, a usage error when there is not,
//! `Creating task in <scope>` on stderr, and the new task's URL alone on
//! stdout so it pipes.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context as _, Result};
use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde_json::{json, Value};

use crate::api::Task;
use crate::cli::{usage_error, Cancelled};
use crate::cmd::{finish, open_in_browser, read_text_file, spinner};
use crate::ctx::Ctx;
use crate::render::time::today;
use crate::resolve::{resolve_assignee, resolve_project};

const PATH: &[&str] = &["task", "create"];

#[derive(clap::Args, Debug)]
#[command(
    about = "Create a new task",
    long_about = "Create a task in Asana.\n\n\
        The task goes into the project named by `--project` (a name, gid or URL).\n\
        Without a project it is created in your workspace and assigned to you, so it\n\
        shows up in My Tasks; pass `--assignee` to give it to someone else.\n\n\
        The `--assignee` flag supports the following special values:\n\
        - `@me`: assign yourself\n\n\
        The `--due` flag accepts `YYYY-MM-DD`, `today`, `tomorrow`, or a weekday name\n\
        for the next such day.",
    after_help = "$ asana task create --title \"I found a bug\" --notes \"Nothing works\"\n\
        $ asana task create --title \"Write release notes\" --project \"Roadmap\"\n\
        $ asana task create --title \"Call the vendor\" --assignee \"@me\" --due tomorrow\n\
        $ asana task create --title \"Review the spec\" --assignee jane@example.com --due friday\n\
        $ asana task create --title \"Plan Q4\" --project 1200000000000000 --due 2026-09-30\n\
        $ asana task create --title \"Spec\" --notes-file spec.md\n\
        $ asana task create --title \"Spec\" --notes-file - < spec.md\n\
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

    /// Supply a description (what Asana calls notes)
    #[arg(
        short = 'b',
        long,
        value_name = "string",
        conflicts_with = "notes_file"
    )]
    pub notes: Option<String>,

    /// Read the description from file (use "-" to read from standard input)
    #[arg(short = 'F', long, value_name = "file")]
    pub notes_file: Option<String>,

    /// Add the task to a project by name, gid or URL
    #[arg(short = 'p', long, value_name = "name")]
    pub project: Option<String>,

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

/// gh prompts only when stdin, stdout and stderr are all terminals.
fn can_prompt(ctx: &Ctx) -> bool {
    ctx.is_tty && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// `? Title ` on stderr, re-asked while the answer is blank. End of input
/// (Ctrl-D) cancels like Ctrl-C does in gh: exit 2, nothing printed.
fn prompt_title(ctx: &Ctx) -> Result<String> {
    let stdin = std::io::stdin();
    loop {
        eprint!("? Title ");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        let read = stdin
            .lock()
            .read_line(&mut line)
            .context("could not read standard input")?;
        if read == 0 {
            eprintln!();
            return Err(Cancelled.into());
        }
        let title = line.trim();
        if !title.is_empty() {
            return Ok(title.to_string());
        }
        let mark = if ctx.color { "\x1b[31mX\x1b[0m" } else { "X" };
        eprintln!("{mark} Sorry, your reply was invalid: a title is required");
    }
}

/// Everything resolved and ready to send: where the task goes (for the
/// `Creating task in ...` line) and the `POST /tasks` body.
struct Prepared {
    scope: String,
    data: Value,
}

async fn prepare(
    ctx: &Ctx,
    args: &Args,
    title: String,
    notes: Option<String>,
    due: Option<NaiveDate>,
) -> Result<Prepared> {
    let ws = ctx.workspace().await?;
    let project = match &args.project {
        Some(p) => Some(resolve_project(ctx, p).await?),
        None => None,
    };
    // A task with neither a project nor an assignee is only reachable through
    // search, so mirror Asana's own quick-add and put it in My Tasks.
    let assignee = match (&args.assignee, &project) {
        (Some(a), _) => Some(resolve_assignee(ctx, a).await?),
        (None, None) => Some(resolve_assignee(ctx, "me").await?),
        (None, Some(_)) => None,
    };

    let mut map = serde_json::Map::new();
    map.insert("name".into(), Value::String(title));
    map.insert("workspace".into(), Value::String(ws.gid.clone()));
    if let Some(n) = notes {
        map.insert("notes".into(), Value::String(n));
    }
    if let Some(a) = &assignee {
        map.insert("assignee".into(), Value::String(a.gid.clone()));
    }
    if let Some(d) = due {
        map.insert("due_on".into(), Value::String(d.to_string()));
    }
    if let Some(p) = &project {
        map.insert("projects".into(), json!([p.gid]));
    }

    let scope = match &project {
        Some(p) => p.display().to_string(),
        None => ws.display().to_string(),
    };
    Ok(Prepared {
        scope,
        data: Value::Object(map),
    })
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
        let sp = spinner(ctx);
        let url = web_url(ctx, args.project.as_deref()).await;
        finish(sp);
        return open_in_browser(&url?);
    }

    let title = match args
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(t) => t.to_string(),
        None if can_prompt(ctx) => prompt_title(ctx)?,
        None => {
            return Err(usage_error(
                PATH,
                "must provide `--title` when not running interactively",
            ))
        }
    };
    let notes = match (&args.notes, &args.notes_file) {
        (Some(n), _) => Some(n.clone()),
        (None, Some(f)) => Some(read_text_file(f)?),
        (None, None) => None,
    };
    let due = args
        .due
        .as_deref()
        .map(|d| parse_due(d, today()))
        .transpose()?;

    let sp = spinner(ctx);
    let prepared = prepare(ctx, &args, title, notes, due).await;
    finish(sp);
    let Prepared { scope, data } = prepared?;

    // Like gh, say where the task is going only when someone is watching.
    if ctx.is_tty {
        eprintln!();
        eprintln!("Creating task in {scope}");
        eprintln!();
    }

    let sp = spinner(ctx);
    let created: Result<Task, _> = ctx.api.post("/tasks", data).await;
    finish(sp);
    let created = created?;
    let url = created
        .permalink_url
        .unwrap_or_else(|| format!("https://app.asana.com/0/0/{}/f", created.gid));
    println!("{url}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("date")
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
        assert!(by_name("notes-file").is_some());
        assert!(by_name("web").is_some_and(|a| a.get_short() == Some('w')));
    }
}
