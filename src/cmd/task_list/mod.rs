//! `asana task list`: tasks assigned to you, or the tasks of a project.

use anyhow::Result;
use serde_json::Value;

use crate::api::{Task, TASK_FIELDS};
use crate::cmd::{finish, open_in_browser, print_json, spinner};
use crate::ctx::Ctx;
use crate::render::table::Cell;
use crate::render::time::{due_phrase, fuzzy_ago, iso, now, today, DueState};
use crate::render::{Palette, Table};
use crate::resolve::{resolve_assignee, resolve_project};

#[derive(clap::Args, Debug)]
#[command(
    about = "List tasks assigned to you or in a project",
    long_about = "List tasks in the workspace. By default, this lists open tasks assigned to you.\n\n\
        With `--project`, list the tasks of that project instead (open only unless `--state` says otherwise).\n\n\
        When stdout is not a terminal the output is tab-separated with no header.",
    after_help = "$ asana task list\n\
        $ asana task list --project \"Roadmap\"\n\
        $ asana task list --assignee \"jane@example.com\"\n\
        $ asana task list --state all --limit 50\n\
        $ asana task list --project Roadmap --json"
)]
pub struct Args {
    /// Filter by project gid, URL or name
    #[arg(short = 'p', long, value_name = "name")]
    pub project: Option<String>,

    /// Filter by assignee: "me", an email, a name or a gid
    #[arg(short = 'a', long, value_name = "login")]
    pub assignee: Option<String>,

    /// Filter by state: {open|completed|all}
    #[arg(short = 's', long, value_name = "string", default_value = "open", value_parser = ["open", "completed", "all"])]
    pub state: String,

    /// Maximum number of tasks to fetch
    #[arg(short = 'L', long, value_name = "int", default_value_t = 30)]
    pub limit: usize,

    /// Output the raw API objects as JSON
    #[arg(long)]
    pub json: bool,

    /// List tasks in the web browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
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
    if args.web {
        finish(sp);
        let url = match &project {
            Some(p) => p
                .permalink_url
                .clone()
                .unwrap_or_else(|| format!("https://app.asana.com/0/{}/list", p.gid)),
            None => format!("https://app.asana.com/0/home/{}", ws.gid),
        };
        return open_in_browser(&url);
    }

    // Who are we filtering by? Default: me, unless a project is given.
    let assignee = match (&args.assignee, &project) {
        (Some(a), _) => match resolve_assignee(ctx, a).await {
            Ok(a) => Some(a),
            Err(e) => {
                finish(sp);
                return Err(e);
            }
        },
        (None, None) => match resolve_assignee(ctx, "me").await {
            Ok(a) => Some(a),
            Err(e) => {
                finish(sp);
                return Err(e);
            }
        },
        (None, Some(_)) => None,
    };
    let me_gid = ctx.me().await.map(|m| m.gid).unwrap_or_default();

    let mut query: Vec<(&str, &str)> = vec![("opt_fields", TASK_FIELDS)];
    let project_gid = project.as_ref().map(|p| p.gid.clone());
    let assignee_gid = assignee.as_ref().map(|a| a.gid.clone());
    match (&project_gid, &assignee_gid) {
        (Some(pg), _) => query.push(("project", pg.as_str())),
        (None, Some(ag)) => {
            query.push(("assignee", ag.as_str()));
            query.push(("workspace", ws.gid.as_str()));
        }
        (None, None) => {}
    }
    if args.state == "open" {
        query.push(("completed_since", "now"));
    }

    let raw: Result<Vec<Value>, _> = ctx.api.get_all("/tasks", &query, None).await;
    finish(sp);
    let raw = raw?;
    let mut items: Vec<(Value, Task)> = raw
        .into_iter()
        .filter_map(|v| {
            serde_json::from_value::<Task>(v.clone())
                .ok()
                .map(|t| (v, t))
        })
        .collect();

    // Client-side filters the API cannot combine.
    if project.is_some() {
        if let Some(a) = &assignee {
            let wanted = if a.gid == "me" {
                me_gid.as_str()
            } else {
                a.gid.as_str()
            };
            items.retain(|(_, t)| t.assignee.as_ref().is_some_and(|x| x.gid == wanted));
        }
    }
    match args.state.as_str() {
        "open" => items.retain(|(_, t)| !t.is_completed()),
        "completed" => items.retain(|(_, t)| t.is_completed()),
        _ => {}
    }
    if project.is_none() {
        items.sort_by(|a, b| b.1.modified_at.cmp(&a.1.modified_at));
    }
    let total = items.len();
    items.truncate(args.limit);

    if args.json {
        let values: Vec<Value> = items.into_iter().map(|(v, _)| v).collect();
        return print_json(&values);
    }

    let assignee_is_me = assignee
        .as_ref()
        .is_some_and(|a| a.gid == "me" || a.gid == me_gid);
    let scope = match (&project, &assignee) {
        (Some(p), Some(a)) if !assignee_is_me => format!("{} assigned to {}", p.display(), a.name),
        (Some(p), Some(_)) => format!("{} assigned to you", p.display()),
        (Some(p), None) => p.display().to_string(),
        (None, Some(a)) if !assignee_is_me => format!("{} assigned to {}", ws.display(), a.name),
        (None, _) => format!("{} assigned to you", ws.display()),
    };
    let has_filters = args.state != "open"
        || (args.assignee.is_some() && !assignee_is_me)
        || (project.is_some() && args.assignee.is_some());

    if items.is_empty() {
        if ctx.is_tty {
            if has_filters {
                eprintln!("no tasks match your search in {scope}");
            } else {
                eprintln!("no open tasks in {scope}");
            }
        }
        return Ok(());
    }

    let p = Palette::new(ctx.color);
    if ctx.is_tty {
        println!();
        if has_filters {
            let verb = if total == 1 { "matches" } else { "match" };
            println!(
                "Showing {} of {} in {scope} that {verb} your search",
                items.len(),
                plural(total, "task")
            );
        } else {
            println!(
                "Showing {} of {} in {scope}",
                items.len(),
                plural(total, "open task")
            );
        }
        println!();
    }

    let third = if project.is_some() {
        "ASSIGNEE"
    } else {
        "PROJECT"
    };
    let mut table = Table::new(
        &["ID", "TITLE", third, "DUE", "UPDATED"],
        ctx.is_tty,
        ctx.width,
        p,
    );
    let now = now();
    let today = today();
    for (_, task) in &items {
        let third_value = if project.is_some() {
            task.assignee
                .as_ref()
                .map(|a| a.display().to_string())
                .unwrap_or_default()
        } else {
            task.projects
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let due_date = task.due_on.or_else(|| {
            task.due_at
                .map(|t| t.with_timezone(&chrono::Local).date_naive())
        });
        let (due, due_state) = match due_date {
            Some(d) if ctx.is_tty => due_phrase(d, today),
            Some(d) => (d.to_string(), DueState::Upcoming),
            None => (String::new(), DueState::Upcoming),
        };
        let updated = match task.modified_at {
            Some(t) if ctx.is_tty => fuzzy_ago(now, t),
            Some(t) => iso(t),
            None => String::new(),
        };
        let id_cell = if task.is_completed() {
            Cell::fixed(task.gid.clone(), |p, s| p.magenta(s))
        } else {
            Cell::fixed(task.gid.clone(), |p, s| p.green(s))
        };
        let due_cell = match due_state {
            DueState::Overdue => Cell::colored(due, |p, s| p.red(s)),
            DueState::Today => Cell::colored(due, |p, s| p.yellow(s)),
            DueState::Upcoming => Cell::plain(due),
        };
        if ctx.is_tty {
            table.add_row(vec![
                id_cell,
                Cell::plain(task.display()),
                Cell::plain(third_value),
                due_cell,
                Cell::colored(updated, |p, s| p.muted(s)),
            ]);
        } else {
            table.add_row(vec![
                Cell::plain(task.gid.clone()),
                Cell::plain(if task.is_completed() {
                    "COMPLETED"
                } else {
                    "OPEN"
                }),
                Cell::plain(task.display()),
                Cell::plain(third_value),
                due_cell,
                Cell::plain(updated),
            ]);
        }
    }
    print!("{}", table.render());
    Ok(())
}
