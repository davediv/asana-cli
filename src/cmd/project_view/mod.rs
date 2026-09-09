//! `asana project view`: one project, laid out like `gh repo view` /
//! `gh issue view`: a bold title, a `state • owner • activity • counts`
//! line, `Label: value` lines, the description rendered with a two-space
//! margin, and a muted permalink footer. Piped: `key:\tvalue` lines, `--`,
//! the raw notes.

mod notes;

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::api::{ApiError, Named, Workspace, PROJECT_FIELDS, TASK_COUNT_FIELDS};
use crate::cmd::{finish, open_in_browser, print_json, spinner};
use crate::ctx::Ctx;
use crate::render::time::{due_phrase, fuzzy_ago, iso, now, short_date, today, DueState};
use crate::render::Palette;
use crate::resolve::resolve_project;

#[derive(clap::Args, Debug)]
#[command(
    about = "View a project",
    long_about = "Display the name, status, owner, task counts and description of a project.\n\n\
        With `--web` flag, open the project in a web browser instead.\n\n\
        A project can be given by gid, by URL, or by name (case-insensitive; a unique prefix is enough).",
    after_help = "$ asana project view Roadmap\n\
        $ asana project view 1200000000000000 --json\n\
        $ asana project view https://app.asana.com/0/1200000000000000/list --web"
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

/// Everything the view shows. Richer than the shared `PROJECT_FIELDS`:
/// `html_notes` for rendering, the latest status update's type and text,
/// member names, and timestamps.
const VIEW_FIELDS: &str = "name,archived,color,created_at,modified_at,due_on,start_on,notes,html_notes,owner.name,team.name,members.name,permalink_url,public,completed,completed_at,current_status.title,current_status.text,current_status.color,current_status.created_at,current_status_update.status_type,current_status_update.title,current_status_update.created_at,workspace.name";

#[derive(Debug, Clone, Default, Deserialize)]
struct Status {
    title: Option<String>,
    #[allow(dead_code)]
    text: Option<String>,
    color: Option<String>,
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct StatusUpdate {
    status_type: Option<String>,
    title: Option<String>,
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct View {
    #[serde(default)]
    gid: String,
    name: Option<String>,
    archived: Option<bool>,
    created_at: Option<DateTime<Utc>>,
    modified_at: Option<DateTime<Utc>>,
    due_on: Option<NaiveDate>,
    #[allow(dead_code)]
    start_on: Option<NaiveDate>,
    notes: Option<String>,
    html_notes: Option<String>,
    owner: Option<Named>,
    team: Option<Named>,
    #[serde(default)]
    members: Vec<Named>,
    permalink_url: Option<String>,
    completed: Option<bool>,
    completed_at: Option<DateTime<Utc>>,
    current_status: Option<Status>,
    current_status_update: Option<StatusUpdate>,
    #[allow(dead_code)]
    workspace: Option<Workspace>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Counts {
    num_tasks: Option<u64>,
    num_incomplete_tasks: Option<u64>,
    num_completed_tasks: Option<u64>,
}

impl View {
    fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.gid)
    }

    fn url(&self) -> String {
        self.permalink_url
            .clone()
            .unwrap_or_else(|| format!("https://app.asana.com/0/{}/list", self.gid))
    }

    fn is_archived(&self) -> bool {
        self.archived.unwrap_or(false)
    }

    fn is_completed(&self) -> bool {
        self.completed.unwrap_or(false)
    }

    /// The latest status update's health (`On track`, `At risk`, ...), from
    /// the status update's type or, failing that, the legacy status colour.
    fn health(&self) -> Option<Health> {
        if let Some(t) = self
            .current_status_update
            .as_ref()
            .and_then(|s| s.status_type.as_deref())
        {
            if let Some(h) = Health::from_type(t) {
                return Some(h);
            }
        }
        self.current_status
            .as_ref()
            .and_then(|s| s.color.as_deref())
            .and_then(Health::from_color)
    }

    fn status_title(&self) -> Option<&str> {
        self.current_status_update
            .as_ref()
            .and_then(|s| s.title.as_deref())
            .or_else(|| self.current_status.as_ref().and_then(|s| s.title.as_deref()))
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }

    fn status_at(&self) -> Option<DateTime<Utc>> {
        self.current_status_update
            .as_ref()
            .and_then(|s| s.created_at)
            .or_else(|| self.current_status.as_ref().and_then(|s| s.created_at))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hue {
    Green,
    Yellow,
    Red,
    Cyan,
    Magenta,
    Muted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Health {
    label: &'static str,
    hue: Hue,
}

impl Health {
    fn from_type(t: &str) -> Option<Self> {
        let (label, hue) = match t {
            "on_track" => ("On track", Hue::Green),
            "at_risk" => ("At risk", Hue::Yellow),
            "off_track" => ("Off track", Hue::Red),
            "on_hold" => ("On hold", Hue::Cyan),
            "complete" => ("Complete", Hue::Magenta),
            "achieved" => ("Achieved", Hue::Green),
            "partial" => ("Partial", Hue::Yellow),
            "missed" => ("Missed", Hue::Red),
            "dropped" => ("Dropped", Hue::Muted),
            _ => return None,
        };
        Some(Self { label, hue })
    }

    fn from_color(c: &str) -> Option<Self> {
        let (label, hue) = match c {
            "green" => ("On track", Hue::Green),
            "yellow" => ("At risk", Hue::Yellow),
            "red" => ("Off track", Hue::Red),
            "blue" => ("On hold", Hue::Cyan),
            "complete" => ("Complete", Hue::Magenta),
            _ => return None,
        };
        Some(Self { label, hue })
    }
}

fn paint(p: &Palette, hue: Hue, s: &str) -> String {
    match hue {
        Hue::Green => p.green(s),
        Hue::Yellow => p.yellow(s),
        Hue::Red => p.red(s),
        Hue::Cyan => p.cyan(s),
        Hue::Magenta => p.magenta(s),
        Hue::Muted => p.muted(s),
    }
}

/// `Active`, `Archived` or `Complete`, like gh's `Open`/`Closed`.
fn state(v: &View) -> (&'static str, Hue) {
    if v.is_archived() {
        ("Archived", Hue::Muted)
    } else if v.is_completed() {
        ("Complete", Hue::Magenta)
    } else {
        ("Active", Hue::Green)
    }
}

/// `10 of 12 tasks open`, `no tasks`.
fn task_summary(c: &Counts) -> Option<String> {
    let total = c.num_tasks?;
    if total == 0 {
        return Some("no tasks".to_string());
    }
    let open = c
        .num_incomplete_tasks
        .or_else(|| c.num_completed_tasks.map(|done| total.saturating_sub(done)))?;
    let noun = if total == 1 { "task" } else { "tasks" };
    Some(format!("{open} of {total} {noun} open"))
}

fn is_gid(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// A project gid from a gid or an app.asana.com URL (`/0/<project>/...`,
/// `/1/<ws>/project/<project>/...`); `None` for anything else (a name).
fn parse_project_gid(input: &str) -> Option<String> {
    let s = input.trim();
    if is_gid(s) {
        return Some(s.to_string());
    }
    let url = Url::parse(s).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let segs: Vec<&str> = url
        .path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if let Some(pos) = segs.iter().position(|s| *s == "project") {
        return segs.get(pos + 1).filter(|g| is_gid(g)).map(|g| g.to_string());
    }
    if segs.first() == Some(&"0") {
        return segs
            .get(1)
            .filter(|g| is_gid(g) && **g != "0")
            .map(|g| g.to_string());
    }
    None
}

/// `GET /projects/{gid}` with the rich field list, falling back to the
/// shared field list should Asana reject one of the names (HTTP 400).
async fn fetch_view(ctx: &Ctx, gid: &str) -> Result<Value, ApiError> {
    let path = format!("/projects/{gid}");
    match ctx.api.get(&path, &[("opt_fields", VIEW_FIELDS)]).await {
        Err(ApiError::Status { status: 400, .. }) => {
            ctx.api.get(&path, &[("opt_fields", PROJECT_FIELDS)]).await
        }
        other => other,
    }
}

async fn fetch_counts(ctx: &Ctx, gid: &str) -> Option<Counts> {
    ctx.api
        .get(
            &format!("/projects/{gid}/task_counts"),
            &[("opt_fields", TASK_COUNT_FIELDS)],
        )
        .await
        .ok()
}

/// Nothing to check before the token is read; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for Args {}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let sp = spinner(ctx);
    let gid = match parse_project_gid(&args.project) {
        Some(gid) => gid,
        None => match resolve_project(ctx, &args.project).await {
            Ok(p) => p.gid,
            Err(e) => {
                finish(sp);
                return Err(e);
            }
        },
    };
    if args.web {
        let raw = fetch_view(ctx, &gid).await;
        finish(sp);
        let view: View = serde_json::from_value(raw?)?;
        return open_in_browser(&view.url());
    }
    if args.json {
        let raw = fetch_view(ctx, &gid).await;
        finish(sp);
        return print_json(&raw?);
    }
    let (raw, counts) = tokio::join!(fetch_view(ctx, &gid), fetch_counts(ctx, &gid));
    finish(sp);
    let view: View = serde_json::from_value(raw?)?;
    let counts = counts.unwrap_or_default();

    if ctx.is_tty {
        print!("{}", render_tty(&view, &counts, ctx.width, ctx.color, now()));
    } else {
        print!("{}", render_piped(&view, &counts));
    }
    Ok(())
}

/// The interactive layout, as one string (every line newline-terminated).
fn render_tty(v: &View, counts: &Counts, width: usize, color: bool, now: DateTime<Utc>) -> String {
    let p = Palette::new(color);
    let mut out = String::new();
    let line = |out: &mut String, s: &str| {
        out.push_str(s);
        out.push('\n');
    };

    line(&mut out, &p.bold(v.display()));

    let mut meta: Vec<String> = Vec::new();
    let (label, hue) = state(v);
    meta.push(paint(&p, hue, label));
    if let Some(owner) = v.owner.as_ref() {
        meta.push(format!("owned by {}", owner.display()));
    }
    if v.is_completed() {
        if let Some(t) = v.completed_at {
            meta.push(format!("completed {}", fuzzy_ago(now, t)));
        }
    } else if let Some(t) = v.modified_at {
        meta.push(format!("updated {}", fuzzy_ago(now, t)));
    } else if let Some(t) = v.created_at {
        meta.push(format!("created {}", fuzzy_ago(now, t)));
    }
    if let Some(summary) = task_summary(counts) {
        meta.push(summary);
    }
    line(&mut out, &meta.join(" • "));

    let health = v.health();
    let title = v.status_title();
    if health.is_some() || title.is_some() {
        let mut s = String::new();
        if let Some(h) = health {
            s.push_str(&paint(&p, h.hue, h.label));
        }
        if let Some(t) = title {
            if !s.is_empty() {
                s.push_str(" — ");
            }
            s.push_str(t);
        }
        if let Some(at) = v.status_at() {
            s.push_str(&format!(" ({})", fuzzy_ago(now, at)));
        }
        line(&mut out, &format!("{}{s}", p.bold("Status: ")));
    }
    if let Some(due) = v.due_on {
        let today = today();
        let (phrase, due_state) = due_phrase(due, today);
        let painted = match due_state {
            DueState::Overdue => p.red(&phrase),
            DueState::Today => p.yellow(&phrase),
            DueState::Upcoming => phrase.clone(),
        };
        let full = short_date(due, today);
        if full == phrase {
            line(&mut out, &format!("{}{painted}", p.bold("Due: ")));
        } else {
            line(&mut out, &format!("{}{painted} ({full})", p.bold("Due: ")));
        }
    }
    if let Some(team) = v.team.as_ref() {
        line(&mut out, &format!("{}{}", p.bold("Team: "), team.display()));
    }
    if !v.members.is_empty() {
        line(&mut out, &format!("{}{}", p.bold("Members: "), member_list(&v.members)));
    }

    out.push('\n');
    out.push('\n');
    let body = notes::render(v.html_notes.as_deref(), v.notes.as_deref(), width, color);
    if body.is_empty() {
        line(&mut out, &format!("  {}", p.muted("No description provided")));
    } else {
        line(&mut out, &body);
    }
    out.push('\n');
    out.push('\n');
    line(
        &mut out,
        &p.muted(&format!("View this project in Asana: {}", v.url())),
    );
    out
}

/// Up to twenty names, then `and N more`.
fn member_list(members: &[Named]) -> String {
    const SHOWN: usize = 20;
    let names: Vec<&str> = members.iter().map(Named::display).collect();
    if names.len() <= SHOWN {
        return names.join(", ");
    }
    format!(
        "{}, and {} more",
        names[..SHOWN].join(", "),
        names.len() - SHOWN
    )
}

/// `key:\tvalue` lines, `--`, then the raw notes, like `gh issue view | cat`.
fn render_piped(v: &View, counts: &Counts) -> String {
    let mut out = String::new();
    let mut kv = |key: &str, value: String| {
        out.push_str(key);
        out.push_str(":\t");
        out.push_str(&value);
        out.push('\n');
    };
    kv("title", v.display().to_string());
    kv("state", state(v).0.to_uppercase());
    kv(
        "owner",
        v.owner.as_ref().map(|o| o.display()).unwrap_or("").to_string(),
    );
    kv(
        "team",
        v.team.as_ref().map(|t| t.display()).unwrap_or("").to_string(),
    );
    kv(
        "members",
        v.members
            .iter()
            .map(Named::display)
            .collect::<Vec<_>>()
            .join(", "),
    );
    kv(
        "status",
        v.health()
            .map(|h| h.label.to_string())
            .or_else(|| v.status_title().map(str::to_string))
            .unwrap_or_default(),
    );
    kv("due", v.due_on.map(|d| d.to_string()).unwrap_or_default());
    kv(
        "tasks",
        counts.num_tasks.map(|n| n.to_string()).unwrap_or_default(),
    );
    kv(
        "open",
        counts
            .num_incomplete_tasks
            .map(|n| n.to_string())
            .unwrap_or_default(),
    );
    kv(
        "completed",
        counts
            .num_completed_tasks
            .map(|n| n.to_string())
            .unwrap_or_default(),
    );
    kv("created", v.created_at.map(iso).unwrap_or_default());
    kv("updated", v.modified_at.map(iso).unwrap_or_default());
    kv("gid", v.gid.clone());
    kv("url", v.url());
    out.push_str("--\n");
    out.push_str(v.notes.as_deref().map(str::trim_end).unwrap_or(""));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> View {
        serde_json::from_value(serde_json::json!({
            "gid": "1200000000000000",
            "name": "Roadmap",
            "archived": false,
            "created_at": "2026-06-01T10:00:00.000Z",
            "modified_at": "2026-09-09T08:00:00.000Z",
            "due_on": "2026-09-30",
            "notes": "Plan for Q4.\nShip the CLI.",
            "html_notes": "<body>Plan for <strong>Q4</strong>.\nShip the CLI.</body>",
            "owner": {"gid": "1", "name": "Div"},
            "team": {"gid": "2", "name": "Engineering"},
            "members": [{"gid": "1", "name": "Div"}, {"gid": "3", "name": "Alice"}],
            "permalink_url": "https://app.asana.com/0/1200000000000000/1200000000000000",
            "current_status": {"title": "Status Update - Sep 5", "color": "green", "created_at": "2026-09-05T09:00:00.000Z"},
            "current_status_update": {"status_type": "on_track", "title": "Status Update - Sep 5", "created_at": "2026-09-05T09:00:00.000Z"}
        }))
        .expect("sample project")
    }

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 9, 10, 0, 0).single().expect("time")
    }

    #[test]
    fn tty_layout_reads_like_gh() {
        let counts = Counts {
            num_tasks: Some(12),
            num_incomplete_tasks: Some(10),
            num_completed_tasks: Some(2),
        };
        let out = render_tty(&sample(), &counts, 80, false, at());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "Roadmap");
        assert_eq!(
            lines[1],
            "Active • owned by Div • updated about 2 hours ago • 10 of 12 tasks open"
        );
        assert_eq!(lines[2], "Status: On track — Status Update - Sep 5 (about 4 days ago)");
        assert!(lines[3].starts_with("Due: "), "{}", lines[3]);
        assert_eq!(lines[4], "Team: Engineering");
        assert_eq!(lines[5], "Members: Div, Alice");
        assert_eq!(lines[6], "");
        assert_eq!(lines[7], "");
        assert_eq!(lines[8], "  Plan for Q4.");
        assert_eq!(lines[9], "  Ship the CLI.");
        assert_eq!(lines[10], "");
        assert_eq!(lines[11], "");
        assert_eq!(
            lines[12],
            "View this project in Asana: https://app.asana.com/0/1200000000000000/1200000000000000"
        );
        assert_eq!(lines.len(), 13);
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn tty_without_extras() {
        let v = View {
            gid: "42".into(),
            name: Some("Bare".into()),
            ..View::default()
        };
        let out = render_tty(&v, &Counts::default(), 80, false, at());
        assert_eq!(
            out,
            "Bare\nActive\n\n\n  No description provided\n\n\nView this project in Asana: https://app.asana.com/0/42/list\n"
        );
    }

    #[test]
    fn colour_placement() {
        let counts = Counts::default();
        let out = render_tty(&sample(), &counts, 80, true, at());
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("\x1b[0;1;39mRoadmap\x1b[0m"), "{:?}", lines[0]);
        assert!(lines[1].starts_with("\x1b[0;32mActive\x1b[0m • owned by Div"), "{:?}", lines[1]);
        assert!(lines[2].starts_with("\x1b[0;1;39mStatus: \x1b[0m\x1b[0;32mOn track\x1b[0m"), "{:?}", lines[2]);
        assert!(lines.last().unwrap().starts_with("\x1b[38;5;242mView this project in Asana:"));
    }

    #[test]
    fn piped_layout() {
        let counts = Counts {
            num_tasks: Some(12),
            num_incomplete_tasks: Some(10),
            num_completed_tasks: Some(2),
        };
        let out = render_piped(&sample(), &counts);
        assert_eq!(
            out,
            "title:\tRoadmap\nstate:\tACTIVE\nowner:\tDiv\nteam:\tEngineering\nmembers:\tDiv, Alice\nstatus:\tOn track\ndue:\t2026-09-30\ntasks:\t12\nopen:\t10\ncompleted:\t2\ncreated:\t2026-06-01T10:00:00Z\nupdated:\t2026-09-09T08:00:00Z\ngid:\t1200000000000000\nurl:\thttps://app.asana.com/0/1200000000000000/1200000000000000\n--\nPlan for Q4.\nShip the CLI.\n"
        );
        assert!(!out.contains("\x1b["));
    }

    #[test]
    fn states_and_summaries() {
        let mut v = sample();
        assert_eq!(state(&v).0, "Active");
        v.completed = Some(true);
        assert_eq!(state(&v).0, "Complete");
        v.archived = Some(true);
        assert_eq!(state(&v).0, "Archived");
        assert_eq!(
            task_summary(&Counts {
                num_tasks: Some(0),
                ..Counts::default()
            })
            .as_deref(),
            Some("no tasks")
        );
        assert_eq!(
            task_summary(&Counts {
                num_tasks: Some(1),
                num_incomplete_tasks: Some(1),
                num_completed_tasks: Some(0),
            })
            .as_deref(),
            Some("1 of 1 task open")
        );
        assert_eq!(task_summary(&Counts::default()), None);
    }

    #[test]
    fn health_falls_back_to_legacy_colour() {
        let mut v = sample();
        v.current_status_update = None;
        assert_eq!(v.health().map(|h| h.label), Some("On track"));
        v.current_status = Some(Status {
            color: Some("red".into()),
            ..Status::default()
        });
        assert_eq!(v.health().map(|h| h.label), Some("Off track"));
        v.current_status = None;
        assert_eq!(v.health(), None);
    }

    #[test]
    fn many_members_are_capped() {
        let members: Vec<Named> = (0..25)
            .map(|i| Named {
                gid: i.to_string(),
                name: Some(format!("m{i}")),
                email: None,
            })
            .collect();
        let s = member_list(&members);
        assert!(s.ends_with("m19, and 5 more"), "{s}");
    }

    #[test]
    fn project_gids_from_urls() {
        assert_eq!(parse_project_gid("123").as_deref(), Some("123"));
        assert_eq!(parse_project_gid(" 123 ").as_deref(), Some("123"));
        assert_eq!(
            parse_project_gid("https://app.asana.com/0/1200000000000000/list").as_deref(),
            Some("1200000000000000")
        );
        assert_eq!(
            parse_project_gid("https://app.asana.com/0/1200000000000000/1211234567890123/f")
                .as_deref(),
            Some("1200000000000000")
        );
        assert_eq!(
            parse_project_gid("https://app.asana.com/1/11/project/1200000000000000/list")
                .as_deref(),
            Some("1200000000000000")
        );
        assert_eq!(parse_project_gid("Roadmap"), None);
        assert_eq!(parse_project_gid("12a"), None);
        assert_eq!(parse_project_gid("https://app.asana.com/0/0/1211234567890123"), None);
        assert_eq!(parse_project_gid("ftp://app.asana.com/0/123/list"), None);
    }
}
