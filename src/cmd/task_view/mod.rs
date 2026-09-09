//! `asana task view`: title, metadata, notes and comments of one task.

use anyhow::Result;
use serde_json::Value;

use crate::api::{Story, Task, STORY_FIELDS, SUBTASK_FIELDS, TASK_VIEW_FIELDS};
use crate::cmd::{finish, open_in_browser, print_json, spinner};
use crate::ctx::Ctx;
use crate::render::text::{indent, render_notes};
use crate::render::time::{
    due_phrase, fuzzy_ago, fuzzy_ago_abbr, iso, long_date, now, today, DueState,
};
use crate::render::Palette;
use crate::resolve::parse_task_id;

#[derive(clap::Args, Debug)]
#[command(
    about = "View a task",
    long_about = "Display the title, notes, and other information about a task.\n\n\
        With `--web` flag, open the task in a web browser instead.",
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

    /// Output the raw API object as JSON
    #[arg(long)]
    pub json: bool,

    /// Open a task in the browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

fn task_url(task: &Task) -> String {
    task.permalink_url
        .clone()
        .unwrap_or_else(|| format!("https://app.asana.com/0/0/{}/f", task.gid))
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    let sp = spinner(ctx);
    let raw: Result<Value, _> = ctx
        .api
        .get(
            &format!("/tasks/{gid}"),
            &[("opt_fields", TASK_VIEW_FIELDS)],
        )
        .await;
    let raw = match raw {
        Ok(v) => v,
        Err(e) => {
            finish(sp);
            return Err(e.into());
        }
    };
    let task: Task = serde_json::from_value(raw.clone())?;
    if args.web {
        finish(sp);
        return open_in_browser(&task_url(&task));
    }
    let stories: Result<Vec<Story>, _> = ctx
        .api
        .get_all(
            &format!("/tasks/{gid}/stories"),
            &[("opt_fields", STORY_FIELDS)],
            None,
        )
        .await;
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
    let comments: Vec<Story> = stories?.into_iter().filter(Story::is_comment).collect();

    if args.json {
        if args.comments {
            let mut obj = raw;
            if let Value::Object(map) = &mut obj {
                map.insert("comments".to_string(), serde_json::to_value(&comments)?);
            }
            return print_json(&obj);
        }
        return print_json(&raw);
    }

    let url = task_url(&task);
    let state = if task.is_completed() {
        "COMPLETED"
    } else {
        "OPEN"
    };
    let author = task
        .created_by
        .as_ref()
        .map(|c| c.display().to_string())
        .unwrap_or_default();
    let assignee = task
        .assignee
        .as_ref()
        .map(|a| a.display().to_string())
        .unwrap_or_default();
    let projects = task.project_labels().join(", ");
    let tags = task
        .tags
        .iter()
        .map(|t| t.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let due_date = task.due_on.or_else(|| {
        task.due_at
            .map(|t| t.with_timezone(&chrono::Local).date_naive())
    });

    if !ctx.is_tty {
        println!("title:\t{}", task.display());
        println!("state:\t{state}");
        println!("author:\t{author}");
        println!("assignee:\t{assignee}");
        println!(
            "due:\t{}",
            due_date.map(|d| d.to_string()).unwrap_or_default()
        );
        println!("projects:\t{projects}");
        println!("tags:\t{tags}");
        println!(
            "parent:\t{}",
            task.parent.as_ref().map(|p| p.display()).unwrap_or("")
        );
        println!(
            "subtasks:\t{}",
            subtasks
                .iter()
                .map(|s| s.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!("comments:\t{}", comments.len());
        println!("created:\t{}", task.created_at.map(iso).unwrap_or_default());
        println!(
            "updated:\t{}",
            task.modified_at.map(iso).unwrap_or_default()
        );
        println!("gid:\t{}", task.gid);
        println!("url:\t{url}");
        println!("--");
        if let Some(notes) = task.notes.as_deref().filter(|n| !n.trim().is_empty()) {
            println!("{notes}");
        }
        if args.comments {
            for c in &comments {
                println!("--");
                println!(
                    "comment:\t{}\t{}",
                    c.created_by.as_ref().map(|a| a.display()).unwrap_or(""),
                    c.created_at.map(iso).unwrap_or_default()
                );
                if let Some(text) = &c.text {
                    println!("{text}");
                }
            }
        }
        return Ok(());
    }

    let p = Palette::new(ctx.color);
    let ws_name = task
        .workspace
        .as_ref()
        .and_then(|w| w.name.clone())
        .or_else(|| ctx.config.workspace.as_ref().map(|w| w.value.clone()))
        .unwrap_or_else(|| "asana".to_string());
    println!(
        "{} {}",
        p.bold(task.display()),
        p.muted(&format!("{ws_name}#{}", task.gid))
    );

    let now = now();
    let mut meta: Vec<String> = Vec::new();
    meta.push(if task.is_completed() {
        p.magenta("Completed")
    } else {
        p.green("Open")
    });
    let created = task
        .created_at
        .map(|t| fuzzy_ago(now, t))
        .unwrap_or_default();
    if author.is_empty() {
        meta.push(format!("created {created}"));
    } else {
        meta.push(format!("{author} created {created}"));
    }
    if let (true, Some(t)) = (task.is_completed(), task.completed_at) {
        meta.push(format!("completed {}", fuzzy_ago(now, t)));
    }
    let n = comments.len();
    meta.push(format!("{n} comment{}", if n == 1 { "" } else { "s" }));
    println!("{}", meta.join(" • "));

    if !assignee.is_empty() {
        println!("{}{assignee}", p.bold("Assignee: "));
    }
    if let Some(d) = due_date {
        let (phrase, state) = due_phrase(d, today());
        let painted = match state {
            DueState::Overdue => p.red(&phrase),
            DueState::Today => p.yellow(&phrase),
            DueState::Upcoming => phrase,
        };
        println!("{}{painted}", p.bold("Due: "));
    }
    if !projects.is_empty() {
        println!("{}{projects}", p.bold("Projects: "));
    }
    if !tags.is_empty() {
        println!("{}{tags}", p.bold("Tags: "));
    }
    if let Some(parent) = &task.parent {
        println!(
            "{}{} ({})",
            p.bold("Parent: "),
            parent.display(),
            parent.gid
        );
    }
    if let Some(n) = task.num_subtasks.filter(|n| *n > 0) {
        let done = subtasks.iter().filter(|s| s.is_completed()).count();
        println!("{}{done} of {n} complete", p.bold("Subtasks: "));
        for s in &subtasks {
            let mark = if s.is_completed() {
                p.green("✓")
            } else {
                p.muted("○")
            };
            println!("  {mark} {} {}", s.display(), p.muted(&s.gid));
        }
    }
    for field in &task.custom_fields {
        if let (Some(name), Some(value)) = (
            &field.name,
            field.display_value.as_deref().filter(|v| !v.is_empty()),
        ) {
            println!("{}{value}", p.bold(&format!("{name}: ")));
        }
    }

    println!();
    println!();
    let notes = render_notes(
        task.html_notes.as_deref(),
        task.notes.as_deref(),
        ctx.width.saturating_sub(4),
    );
    if notes.is_empty() {
        println!("  {}", p.muted("No description provided"));
    } else {
        println!("{}", indent(&notes, 2));
    }
    println!();
    println!();

    if !comments.is_empty() {
        let shown: Vec<&Story> = if args.comments {
            comments.iter().collect()
        } else {
            comments.iter().rev().take(1).collect()
        };
        if !args.comments && comments.len() > 1 {
            let hidden = comments.len() - 1;
            println!(
                "{}",
                p.muted(&format!(
                    "———————— Not showing {hidden} comment{} ————————",
                    if hidden == 1 { "" } else { "s" }
                ))
            );
            println!();
            println!();
        }
        let last = shown.len() - 1;
        for (i, c) in shown.iter().enumerate() {
            let who = c
                .created_by
                .as_ref()
                .map(|a| a.display().to_string())
                .unwrap_or_default();
            let mut header = p.bold(&who);
            if i == last {
                let when = c
                    .created_at
                    .map(|t| fuzzy_ago_abbr(now, t))
                    .unwrap_or_default();
                header.push_str(&p.bold(&format!(" • {when}")));
                header.push_str(&p.bold(" • "));
                header.push_str(&p.bold_cyan("Newest comment"));
            } else {
                let when = c.created_at.map(long_date).unwrap_or_default();
                header.push_str(&p.bold(&format!(" • {when}")));
            }
            if c.is_pinned.unwrap_or(false) {
                header.push_str(&p.bold(" • Pinned"));
            }
            println!("{header}");
            if let Some(likes) = c.num_likes.filter(|n| *n > 0) {
                println!("{likes} 👍");
            }
            println!();
            let body = render_notes(
                c.html_text.as_deref(),
                c.text.as_deref(),
                ctx.width.saturating_sub(4),
            );
            println!("{}", indent(&body, 2));
            println!();
            if args.comments && i != last {
                println!();
            }
        }
        println!();
        if !args.comments && comments.len() > 1 {
            println!(
                "{}",
                p.muted("Use --comments to view the full conversation")
            );
        }
    }
    println!("{}", p.muted(&format!("View this task in Asana: {url}")));
    Ok(())
}
