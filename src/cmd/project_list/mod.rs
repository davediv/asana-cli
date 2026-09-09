//! `asana project list`: the projects in the workspace, gh `repo list` style.

use anyhow::Result;
use serde_json::Value;

use crate::api::{Project, PROJECT_FIELDS};
use crate::cmd::{finish, open_in_browser, print_json, spinner};
use crate::ctx::Ctx;
use crate::render::table::Cell;
use crate::render::time::{due_phrase, fuzzy_ago, iso, now, today, DueState};
use crate::render::{Palette, Table};

#[derive(clap::Args, Debug)]
#[command(
    about = "List projects in the workspace",
    long_about = "List projects in the workspace. By default, this only lists projects that are not archived.\n\n\
        When stdout is not a terminal the output is tab-separated with no header.",
    after_help = "$ asana project list\n$ asana project list --archived\n$ asana project list --limit 5 --json"
)]
pub struct Args {
    /// Include archived projects
    #[arg(long)]
    pub archived: bool,

    /// Maximum number of projects to fetch
    #[arg(short = 'L', long, value_name = "int", default_value_t = 30)]
    pub limit: usize,

    /// Output the raw API objects as JSON
    #[arg(long)]
    pub json: bool,

    /// List projects in the web browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

/// Which status colour Asana reports: green / yellow / red / blue / complete.
fn status_cell(project: &Project) -> Cell {
    let status = project.current_status.as_ref();
    let title = status
        .and_then(|s| s.title.clone())
        .or_else(|| {
            status.and_then(|s| s.color.as_deref()).map(|c| match c {
                "green" => "On track".to_string(),
                "yellow" => "At risk".to_string(),
                "red" => "Off track".to_string(),
                "blue" => "On hold".to_string(),
                "complete" => "Complete".to_string(),
                other => other.to_string(),
            })
        })
        .unwrap_or_default();
    match status.and_then(|s| s.color.as_deref()) {
        Some("green") => Cell::colored(title, |p, s| p.green(s)),
        Some("yellow") => Cell::colored(title, |p, s| p.yellow(s)),
        Some("red") => Cell::colored(title, |p, s| p.red(s)),
        Some("blue") => Cell::colored(title, |p, s| p.cyan(s)),
        Some("complete") => Cell::colored(title, |p, s| p.magenta(s)),
        _ => Cell::plain(title),
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let ws = ctx.workspace().await?;
    if args.web {
        return open_in_browser(&format!("https://app.asana.com/0/home/{}", ws.gid));
    }
    let limit = args.limit.max(1);
    let mut query: Vec<(&str, &str)> = vec![
        ("workspace", ws.gid.as_str()),
        ("opt_fields", PROJECT_FIELDS),
    ];
    if !args.archived {
        query.push(("archived", "false"));
    }
    let sp = spinner(ctx);
    if args.json {
        let projects: Result<Vec<Value>, _> =
            ctx.api.get_all("/projects", &query, Some(limit)).await;
        finish(sp);
        return print_json(&projects?);
    }
    let projects: Result<Vec<Project>, _> = ctx.api.get_all("/projects", &query, None).await;
    finish(sp);
    let mut projects = projects?;
    let total = projects.len();
    projects.truncate(limit);

    if projects.is_empty() {
        if ctx.is_tty {
            eprintln!("no projects in {}", ws.display());
        }
        return Ok(());
    }

    let p = Palette::new(ctx.color);
    if ctx.is_tty {
        let what = if total == 1 { "project" } else { "projects" };
        println!();
        println!(
            "Showing {} of {total} {what} in {}",
            projects.len(),
            ws.display()
        );
        println!();
    }
    let now = now();
    let today = today();
    let mut table = Table::new(
        &["NAME", "STATUS", "OWNER", "DUE", "UPDATED"],
        ctx.is_tty,
        ctx.width,
        p,
    );
    for project in &projects {
        let (due, due_state) = match project.due_on {
            Some(d) if ctx.is_tty => due_phrase(d, today),
            Some(d) => (d.to_string(), DueState::Upcoming),
            None => (String::new(), DueState::Upcoming),
        };
        let due_cell = match due_state {
            DueState::Overdue => Cell::colored(due, |p, s| p.red(s)),
            DueState::Today => Cell::colored(due, |p, s| p.yellow(s)),
            DueState::Upcoming => Cell::plain(due),
        };
        let updated = match project.modified_at {
            Some(t) if ctx.is_tty => fuzzy_ago(now, t),
            Some(t) => iso(t),
            None => String::new(),
        };
        let mut name = project.display().to_string();
        if project.archived.unwrap_or(false) && ctx.is_tty {
            name.push_str(" (archived)");
        }
        let status = if ctx.is_tty {
            status_cell(project)
        } else {
            Cell::plain(
                project
                    .current_status
                    .as_ref()
                    .and_then(|s| s.color.clone())
                    .unwrap_or_default(),
            )
        };
        let row = if ctx.is_tty {
            vec![
                Cell::fixed(name, |p, s| p.bold(s)),
                status,
                Cell::plain(
                    project
                        .owner
                        .as_ref()
                        .map(|o| o.display().to_string())
                        .unwrap_or_default(),
                ),
                due_cell,
                Cell::colored(updated, |p, s| p.muted(s)),
            ]
        } else {
            vec![
                Cell::plain(project.gid.clone()),
                Cell::plain(name),
                status,
                Cell::plain(
                    project
                        .owner
                        .as_ref()
                        .map(|o| o.display().to_string())
                        .unwrap_or_default(),
                ),
                due_cell,
                Cell::plain(updated),
            ]
        };
        table.add_row(row);
    }
    print!("{}", table.render());
    Ok(())
}
