//! `asana task create`: create a task in the workspace or a project.

use anyhow::{bail, Result};
use chrono::{Duration, NaiveDate};
use serde_json::{json, Value};

use crate::api::Task;
use crate::cli::usage_error;
use crate::cmd::{can_prompt, finish, open_in_browser, prompt_line, read_text_file, spinner};
use crate::ctx::Ctx;
use crate::render::time::today;
use crate::resolve::{resolve_assignee, resolve_project};

const PATH: &[&str] = &["task", "create"];

#[derive(clap::Args, Debug)]
#[command(
    about = "Create a new task",
    long_about = "Create a task in the workspace, optionally in a project.\n\n\
        The `--assignee` flag supports the following special values:\n\
        - `@me`: assign yourself\n\n\
        The `--due` flag accepts `YYYY-MM-DD`, `today` or `tomorrow`.",
    after_help = "$ asana task create --title \"I found a bug\" --notes \"Nothing works\"\n\
        $ asana task create --title \"Write release notes\" --project \"Roadmap\"\n\
        $ asana task create --title \"Call the vendor\" --assignee \"@me\" --due tomorrow\n\
        $ asana task create --title \"Spec\" --notes-file spec.md\n\
        $ asana task create --title \"Spec\" --notes-file - < spec.md"
)]
pub struct Args {
    #[arg(
        short = 't',
        long,
        value_name = "string",
        help = "Supply a title. Will prompt for one otherwise."
    )]
    pub title: Option<String>,

    /// Supply the notes (description)
    #[arg(
        short = 'b',
        long,
        value_name = "string",
        conflicts_with = "notes_file"
    )]
    pub notes: Option<String>,

    /// Read notes from file (use "-" to read from standard input)
    #[arg(short = 'F', long, value_name = "file")]
    pub notes_file: Option<String>,

    /// Add the task to a project by gid, URL or name
    #[arg(short = 'p', long, value_name = "name")]
    pub project: Option<String>,

    #[arg(
        short = 'a',
        long,
        value_name = "login",
        help = "Assign a person by email, name or gid. Use \"@me\" to self-assign."
    )]
    pub assignee: Option<String>,

    /// Due date: YYYY-MM-DD, "today" or "tomorrow"
    #[arg(short = 'd', long, value_name = "date")]
    pub due: Option<String>,

    /// Open the browser to create a task
    #[arg(short = 'w', long)]
    pub web: bool,
}

/// `YYYY-MM-DD`, `today`, `tomorrow`.
pub fn parse_due(input: &str, today: NaiveDate) -> Result<NaiveDate> {
    match input.trim().to_lowercase().as_str() {
        "today" => Ok(today),
        "tomorrow" => Ok(today + Duration::days(1)),
        s => NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
            anyhow::anyhow!("invalid due date \"{input}\": expected YYYY-MM-DD, today or tomorrow")
        }),
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let title = match args
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(t) => t.to_string(),
        None if args.web => String::new(),
        None => {
            if !can_prompt(ctx) {
                return Err(usage_error(
                    PATH,
                    "must provide `--title` when not running interactively",
                ));
            }
            let t = prompt_line("Title")?;
            if t.is_empty() {
                bail!("title cannot be blank");
            }
            t
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

    let ws = ctx.workspace().await?;
    let sp = spinner(ctx);
    let project = match &args.project {
        Some(p) => match resolve_project(ctx, p).await {
            Ok(p) => Some(p),
            Err(e) => {
                finish(sp);
                return Err(e);
            }
        },
        None => None,
    };
    finish(sp);

    if args.web {
        let url = match &project {
            Some(p) => p
                .permalink_url
                .clone()
                .unwrap_or_else(|| format!("https://app.asana.com/0/{}/list", p.gid)),
            None => format!("https://app.asana.com/0/home/{}", ws.gid),
        };
        return open_in_browser(&url);
    }

    let sp = spinner(ctx);
    let assignee = match &args.assignee {
        Some(a) => match resolve_assignee(ctx, a).await {
            Ok(a) => Some(a),
            Err(e) => {
                finish(sp);
                return Err(e);
            }
        },
        None => None,
    };

    let mut data = json!({
        "name": title,
        "workspace": ws.gid,
    });
    if let Value::Object(map) = &mut data {
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
    }
    finish(sp);

    let scope = project
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ws.display().to_string());
    eprintln!();
    eprintln!("Creating task in {scope}");
    eprintln!();

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

    #[test]
    fn parses_due_dates() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 9).expect("date");
        assert_eq!(parse_due("today", today).unwrap(), today);
        assert_eq!(
            parse_due("Tomorrow", today).unwrap(),
            today + Duration::days(1)
        );
        assert_eq!(
            parse_due("2026-12-01", today).unwrap(),
            NaiveDate::from_ymd_opt(2026, 12, 1).expect("date")
        );
        let err = parse_due("next week", today).unwrap_err().to_string();
        assert_eq!(
            err,
            "invalid due date \"next week\": expected YYYY-MM-DD, today or tomorrow"
        );
    }
}
