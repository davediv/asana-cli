//! `asana project view`: one project's header block, notes and permalink.

use anyhow::Result;
use serde_json::Value;

use crate::api::{Project, TaskCounts, PROJECT_FIELDS, TASK_COUNT_FIELDS};
use crate::cmd::{finish, open_in_browser, print_json, spinner};
use crate::ctx::Ctx;
use crate::render::text::{indent, render_notes};
use crate::render::time::{due_phrase, fuzzy_ago, iso, now, short_date, today, DueState};
use crate::render::Palette;
use crate::resolve::resolve_project;

#[derive(clap::Args, Debug)]
#[command(
    about = "View a project",
    long_about = "Display the name, status, owner, task counts and notes of a project.\n\n\
        With `--web` flag, open the project in a web browser instead.",
    after_help = "$ asana project view Roadmap\n$ asana project view 1200000000000000 --json\n$ asana project view https://app.asana.com/0/1200000000000000/list --web"
)]
pub struct Args {
    /// Project gid, URL or name
    #[arg(value_name = "project")]
    pub project: String,

    /// Output the raw API object as JSON
    #[arg(long)]
    pub json: bool,

    /// Open the project in the browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

fn status_line(project: &Project, p: &Palette) -> Option<String> {
    let status = project.current_status.as_ref()?;
    let title = status.title.clone().or_else(|| status.color.clone())?;
    let painted = match status.color.as_deref() {
        Some("green") => p.green(&title),
        Some("yellow") => p.yellow(&title),
        Some("red") => p.red(&title),
        Some("blue") => p.cyan(&title),
        Some("complete") => p.magenta(&title),
        _ => title,
    };
    Some(painted)
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let sp = spinner(ctx);
    let project = resolve_project(ctx, &args.project).await;
    let project = match project {
        Ok(p) => p,
        Err(e) => {
            finish(sp);
            return Err(e);
        }
    };
    if args.web {
        finish(sp);
        let url = project
            .permalink_url
            .clone()
            .unwrap_or_else(|| format!("https://app.asana.com/0/{}/list", project.gid));
        return open_in_browser(&url);
    }
    if args.json {
        let raw: Result<Value, _> = ctx
            .api
            .get(
                &format!("/projects/{}", project.gid),
                &[("opt_fields", PROJECT_FIELDS)],
            )
            .await;
        finish(sp);
        return print_json(&raw?);
    }
    let counts: Result<TaskCounts, _> = ctx
        .api
        .get(
            &format!("/projects/{}/task_counts", project.gid),
            &[("opt_fields", TASK_COUNT_FIELDS)],
        )
        .await;
    finish(sp);
    let counts = counts.unwrap_or_default();
    let p = Palette::new(ctx.color);
    let url = project
        .permalink_url
        .clone()
        .unwrap_or_else(|| format!("https://app.asana.com/0/{}/list", project.gid));

    if !ctx.is_tty {
        let notes = project.notes.clone().unwrap_or_default();
        println!("name:\t{}", project.display());
        println!("gid:\t{}", project.gid);
        println!(
            "team:\t{}",
            project.team.as_ref().map(|t| t.display()).unwrap_or("")
        );
        println!(
            "owner:\t{}",
            project.owner.as_ref().map(|o| o.display()).unwrap_or("")
        );
        println!(
            "status:\t{}",
            project
                .current_status
                .as_ref()
                .and_then(|s| s.title.clone().or_else(|| s.color.clone()))
                .unwrap_or_default()
        );
        println!(
            "due:\t{}",
            project.due_on.map(|d| d.to_string()).unwrap_or_default()
        );
        println!("archived:\t{}", project.archived.unwrap_or(false));
        println!("tasks:\t{}", counts.num_tasks.unwrap_or(0));
        println!("incomplete:\t{}", counts.num_incomplete_tasks.unwrap_or(0));
        println!("completed:\t{}", counts.num_completed_tasks.unwrap_or(0));
        println!(
            "updated:\t{}",
            project.modified_at.map(iso).unwrap_or_default()
        );
        println!("url:\t{url}");
        println!("--");
        if !notes.is_empty() {
            println!("{notes}");
        }
        return Ok(());
    }

    println!("{}", p.bold(project.display()));
    let mut meta: Vec<String> = Vec::new();
    if let Some(team) = project.team.as_ref() {
        meta.push(team.display().to_string());
    }
    if let Some(owner) = project.owner.as_ref() {
        meta.push(format!("owned by {}", owner.display()));
    }
    let open = counts.num_incomplete_tasks.unwrap_or(0);
    let total = counts.num_tasks.unwrap_or(0);
    meta.push(format!("{open} open of {total} tasks"));
    if let Some(t) = project.modified_at {
        meta.push(format!("updated {}", fuzzy_ago(now(), t)));
    }
    println!("{}", meta.join(" • "));
    if let Some(status) = status_line(&project, &p) {
        println!("{}{status}", p.bold("Status: "));
    }
    if let Some(due) = project.due_on {
        let today = today();
        let (phrase, state) = due_phrase(due, today);
        let painted = match state {
            DueState::Overdue => p.red(&phrase),
            DueState::Today => p.yellow(&phrase),
            DueState::Upcoming => phrase,
        };
        let full = short_date(due, today);
        if full == painted {
            println!("{}{painted}", p.bold("Due: "));
        } else {
            println!("{}{painted} ({full})", p.bold("Due: "));
        }
    }
    if project.archived.unwrap_or(false) {
        println!("{}", p.muted("Archived"));
    }
    let notes = render_notes(
        project.html_notes.as_deref(),
        project.notes.as_deref(),
        ctx.width.saturating_sub(4),
    );
    if !notes.is_empty() {
        println!();
        println!();
        println!("{}", indent(&notes, 2));
    } else {
        println!();
        println!("  {}", p.muted("No description provided"));
    }
    println!();
    println!();
    println!("{}", p.muted(&format!("View this project in Asana: {url}")));
    Ok(())
}
