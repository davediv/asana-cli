//! `asana project list`: the projects in the workspace, in the form of
//! `gh repo list`.
//!
//! On a terminal:
//!
//! ```text
//! Showing 8 of 8 projects in Acme Inc
//!
//! NAME              DESCRIPTION                              INFO               UPDATED
//! Website redesign  Everything about the website relaunch.   public, on track   about 39 minutes ago
//! ```
//!
//! NAME is bold, INFO and UPDATED are muted (INFO yellow when the project is
//! private), rows are newest-modified first, and the widest column gives way
//! to `...` first. Piped: `gid  name  description  info  modified` separated
//! by tabs, no header, ISO-8601 time.

mod table;

use anyhow::Result;
use serde_json::Value;

use crate::api::{Project, PROJECT_FIELDS};
use crate::cmd::{finish, open_in_browser, print_json, spinner};
use crate::ctx::Ctx;
use crate::render::time::{fuzzy_ago, iso, now};
use table::{Cell, Style, Table};

#[derive(clap::Args, Debug)]
#[command(
    about = "List projects in the workspace",
    long_about = "List projects in the workspace. By default, this only lists projects that\n\
        are not archived.\n\n\
        Projects are shown most recently modified first. The INFO column carries the\n\
        project's visibility (public or private), its latest status update (on track,\n\
        at risk, off track, on hold, complete) and whether it is archived.\n\n\
        A name from the NAME column is accepted by every command that takes a project,\n\
        for example `asana task list --project \"Roadmap\"`; a case-insensitive unique\n\
        prefix is enough.\n\n\
        When stdout is not a terminal the output is tab-separated with no header:\n\
        gid, name, description, info, modified time.",
    after_help = "$ asana project list\n\
        $ asana project list --archived\n\
        $ asana project list --limit 5\n\
        $ asana project list --json\n\
        $ asana project list | cut -f1,2"
)]
pub struct Args {
    /// Show only archived projects
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

/// One project as it came off the wire plus its decoded form: the JSON is
/// what `--json` prints, the struct is what the table reads.
struct Item {
    raw: Value,
    project: Project,
}

impl Item {
    fn from_value(raw: Value) -> Self {
        let project = serde_json::from_value::<Project>(raw.clone()).unwrap_or_else(|_| {
            // A field in a shape we did not expect: keep the row rather than
            // drop it, with whatever we can read directly.
            Project {
                gid: raw["gid"].as_str().unwrap_or_default().to_string(),
                name: raw["name"].as_str().map(str::to_string),
                ..Project::default()
            }
        });
        Self { raw, project }
    }
}

/// gh's `text.RemoveExcessiveWhitespace`: trimmed, every run of whitespace
/// (newlines included) collapsed to one space, so notes fit on a table row.
fn description(project: &Project) -> String {
    project
        .notes
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The INFO column, in the style of gh's `public, fork, archived`:
/// visibility, then the latest status update's colour as words, then
/// `complete` and `archived` flags.
fn info(project: &Project) -> String {
    let mut tags: Vec<&str> = Vec::new();
    match project.public {
        Some(true) => tags.push("public"),
        Some(false) => tags.push("private"),
        None => {}
    }
    let status = project
        .current_status
        .as_ref()
        .and_then(|s| s.color.as_deref());
    match status {
        Some("green") => tags.push("on track"),
        Some("yellow") => tags.push("at risk"),
        Some("red") => tags.push("off track"),
        Some("blue") => tags.push("on hold"),
        Some("complete") => tags.push("complete"),
        _ => {}
    }
    if project.completed.unwrap_or(false) && !tags.contains(&"complete") {
        tags.push("complete");
    }
    if project.archived.unwrap_or(false) {
        tags.push("archived");
    }
    tags.join(", ")
}

/// gh paints INFO yellow for private repositories and muted otherwise.
fn info_style(project: &Project) -> Style {
    if project.public == Some(false) {
        Style::Yellow
    } else {
        Style::Muted
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// gh's `listHeader`: the line above the table, or the whole output when
/// there is nothing to list.
fn header_line(workspace: &str, shown: usize, total: usize, has_filters: bool) -> String {
    if total == 0 {
        return if has_filters {
            "No results match your search".to_string()
        } else {
            format!("There are no projects in {workspace}")
        };
    }
    let filter = if has_filters {
        " that match your search"
    } else {
        ""
    };
    format!(
        "Showing {shown} of {} in {workspace}{filter}",
        plural(total, "project")
    )
}

/// Most recently modified first; ties and unknown times keep API order.
fn sort_newest_first(items: &mut [Item]) {
    items.sort_by(|a, b| b.project.modified_at.cmp(&a.project.modified_at));
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let ws = ctx.workspace().await?;
    if args.web {
        return open_in_browser(&format!("https://app.asana.com/0/home/{}", ws.gid));
    }
    let archived = if args.archived { "true" } else { "false" };
    let query = [
        ("workspace", ws.gid.as_str()),
        ("archived", archived),
        ("opt_fields", PROJECT_FIELDS),
    ];

    // Everything, not just `limit`: the summary line says how many there are
    // in total, and the newest-first order needs the whole set.
    let sp = spinner(ctx);
    let raw: Result<Vec<Value>, _> = ctx.api.get_all("/projects", &query, None).await;
    finish(sp);
    let mut items: Vec<Item> = raw?.into_iter().map(Item::from_value).collect();
    sort_newest_first(&mut items);
    let total = items.len();
    items.truncate(args.limit.max(1));

    if args.json {
        let values: Vec<Value> = items.into_iter().map(|i| i.raw).collect();
        return print_json(&values);
    }

    if ctx.is_tty {
        println!();
        println!(
            "{}",
            header_line(ws.display(), items.len(), total, args.archived)
        );
        println!();
    }
    if items.is_empty() {
        return Ok(());
    }

    if !ctx.is_tty {
        let mut out = String::new();
        for item in &items {
            let p = &item.project;
            let modified = p.modified_at.map(iso).unwrap_or_default();
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                p.gid,
                p.display(),
                description(p),
                info(p),
                modified
            ));
        }
        print!("{out}");
        return Ok(());
    }

    let now = now();
    let mut table = Table::new(
        &["NAME", "DESCRIPTION", "INFO", "UPDATED"],
        ctx.width,
        ctx.color,
    );
    for item in &items {
        let p = &item.project;
        let updated = p.modified_at.map(|t| fuzzy_ago(now, t)).unwrap_or_default();
        table.add_row(vec![
            Cell::new(p.display(), Style::Bold),
            Cell::new(description(p), Style::Plain),
            Cell::new(info(p), info_style(p)),
            Cell::new(updated, Style::Muted),
        ]);
    }
    print!("{}", table.render());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ProjectStatus;
    use serde_json::json;

    fn project(v: Value) -> Project {
        serde_json::from_value(v).expect("project json")
    }

    #[test]
    fn description_collapses_whitespace_like_gh() {
        let p = project(json!({"gid": "1", "notes": "  Everything about\n\nthe   relaunch.\t"}));
        assert_eq!(description(&p), "Everything about the relaunch.");
        assert_eq!(description(&project(json!({"gid": "1"}))), "");
        assert_eq!(description(&project(json!({"gid": "1", "notes": ""}))), "");
    }

    #[test]
    fn info_reads_like_gh_repo_flags() {
        let p = project(json!({
            "gid": "1", "public": true,
            "current_status": {"title": "All good", "color": "green"}
        }));
        assert_eq!(info(&p), "public, on track");
        assert_eq!(info_style(&p), Style::Muted);

        let p = project(json!({
            "gid": "1", "public": false, "archived": true,
            "current_status": {"color": "red"}
        }));
        assert_eq!(info(&p), "private, off track, archived");
        assert_eq!(info_style(&p), Style::Yellow);

        let p = project(json!({"gid": "1", "public": true, "completed": true,
            "current_status": {"color": "complete"}}));
        assert_eq!(info(&p), "public, complete");

        let p = project(json!({"gid": "1", "public": true, "completed": true}));
        assert_eq!(info(&p), "public, complete");

        let p = project(json!({"gid": "1", "public": true, "current_status": {"color": "blue"}}));
        assert_eq!(info(&p), "public, on hold");
        let p = project(json!({"gid": "1", "public": true, "current_status": {"color": "yellow"}}));
        assert_eq!(info(&p), "public, at risk");

        // nothing known at all
        assert_eq!(info(&project(json!({"gid": "1"}))), "");
        assert_eq!(info_style(&project(json!({"gid": "1"}))), Style::Muted);
    }

    #[test]
    fn header_line_matches_gh_wording() {
        assert_eq!(
            header_line("Acme Inc", 8, 8, false),
            "Showing 8 of 8 projects in Acme Inc"
        );
        assert_eq!(
            header_line("Acme Inc", 3, 8, false),
            "Showing 3 of 8 projects in Acme Inc"
        );
        assert_eq!(
            header_line("Acme Inc", 1, 1, false),
            "Showing 1 of 1 project in Acme Inc"
        );
        assert_eq!(
            header_line("Acme Inc", 2, 2, true),
            "Showing 2 of 2 projects in Acme Inc that match your search"
        );
        assert_eq!(
            header_line("Acme Inc", 0, 0, false),
            "There are no projects in Acme Inc"
        );
        assert_eq!(
            header_line("Acme Inc", 0, 0, true),
            "No results match your search"
        );
    }

    #[test]
    fn newest_first_keeps_api_order_for_ties_and_unknowns() {
        let mk = |gid: &str, at: Option<&str>| {
            let mut v = json!({"gid": gid});
            if let Some(at) = at {
                v["modified_at"] = json!(at);
            }
            Item::from_value(v)
        };
        let mut items = vec![
            mk("a", Some("2026-09-01T00:00:00.000Z")),
            mk("b", None),
            mk("c", Some("2026-09-09T00:00:00.000Z")),
            mk("d", Some("2026-09-09T00:00:00.000Z")),
            mk("e", None),
        ];
        sort_newest_first(&mut items);
        let order: Vec<&str> = items.iter().map(|i| i.project.gid.as_str()).collect();
        assert_eq!(order, vec!["c", "d", "a", "b", "e"]);
    }

    #[test]
    fn unexpected_shapes_keep_the_row() {
        // due_on in a shape serde cannot read: the row survives with its name.
        let item = Item::from_value(json!({"gid": "77", "name": "Odd", "due_on": 12345}));
        assert_eq!(item.project.gid, "77");
        assert_eq!(item.project.display(), "Odd");
        assert_eq!(item.raw["due_on"], json!(12345));
    }

    #[test]
    fn status_words_cover_every_asana_colour() {
        for (color, word) in [
            ("green", "on track"),
            ("yellow", "at risk"),
            ("red", "off track"),
            ("blue", "on hold"),
            ("complete", "complete"),
        ] {
            let p = Project {
                gid: "1".into(),
                public: Some(true),
                current_status: Some(ProjectStatus {
                    color: Some(color.into()),
                    ..ProjectStatus::default()
                }),
                ..Project::default()
            };
            assert_eq!(info(&p), format!("public, {word}"));
        }
    }
}
