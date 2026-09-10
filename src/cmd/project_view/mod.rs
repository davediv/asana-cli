//! `asana project view`: one project, laid out like `gh repo view` /
//! `gh issue view`: a bold name followed by the gid that names it, a
//! `state • visibility • owner • activity • counts` line, `Label: value`
//! lines, the description with a two-space margin, and a muted permalink
//! footer. Piped: `key:\tvalue` lines, `--`, the raw notes.
//! `--json [fields]` and `--jq` are gh's, through `render::json`.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::api::{ApiError, Named, Project, PROJECT_FIELDS, TASK_COUNT_FIELDS};
use crate::cmd::{open_in_browser, with_spinner};
use crate::ctx::Ctx;
use crate::render::json::{self, JsonArgs};
use crate::render::notes::{render_lines, Layout};
use crate::render::time::{due_phrase, fuzzy_ago, iso, now, short_date, today, DueState};
use crate::render::Palette;
use crate::resolve::{parse_project_gid, resolve_project};

/// Every top-level key `--json` can print: what `VIEW_FIELDS` asks Asana
/// for (plus `gid`, which always comes back) and `task_counts`, the
/// `/task_counts` object folded in so a script gets the numbers the view
/// shows without a second call.
const JSON_FIELDS: &[&str] = &[
    "gid",
    "name",
    "archived",
    "color",
    "completed",
    "completed_at",
    "created_at",
    "modified_at",
    "due_on",
    "start_on",
    "notes",
    "html_notes",
    "owner",
    "team",
    "members",
    "permalink_url",
    "public",
    "privacy_setting",
    "current_status",
    "current_status_update",
    "workspace",
    "task_counts",
];

#[derive(clap::Args, Debug)]
#[command(
    about = "View a project",
    long_about = "Display the name, status, owner, task counts and description of a project.\n\n\
        A project can be given by gid, by URL, or by name. Names are matched\n\
        case-insensitively, a unique prefix or a whole word is enough, and a leading\n\
        emoji may be left out (`UTL` finds `⚙️ UTL`).\n\n\
        With `--web`, open the project in a web browser instead.\n\n\
        For more information about output formatting flags, see `asana help formatting`.",
    after_help = "$ asana project view Roadmap\n\
        $ asana project view 1200000000000000\n\
        $ asana project view https://app.asana.com/0/1200000000000000/list\n\
        $ asana project view Roadmap --json name,owner,task_counts\n\
        $ asana project view Roadmap --json permalink_url --jq .permalink_url\n\
        $ asana project view Roadmap --web"
)]
pub struct Args {
    /// Project gid, URL or name
    #[arg(value_name = "project")]
    pub project: String,

    #[command(flatten)]
    pub json: JsonArgs,

    /// Open a project in the browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

/// Everything the view shows. Richer than the shared `PROJECT_FIELDS`:
/// `html_notes` for rendering, the latest status update's type and text,
/// member names, and timestamps.
const VIEW_FIELDS: &str = "name,archived,color,created_at,modified_at,due_on,start_on,notes,html_notes,owner.name,team.name,members.name,permalink_url,public,privacy_setting,completed,completed_at,current_status.title,current_status.text,current_status.color,current_status.created_at,current_status_update.status_type,current_status_update.title,current_status_update.created_at,workspace.name";

#[derive(Debug, Clone, Default, Deserialize)]
struct Status {
    title: Option<String>,
    color: Option<String>,
    created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct StatusUpdate {
    status_type: Option<String>,
    title: Option<String>,
    created_at: Option<DateTime<Utc>>,
}

/// The shared `Project` plus what only the view asks for. `current_status`
/// is declared here (with its `created_at`) so it is taken before the rest
/// of the object flattens into `project`.
#[derive(Debug, Clone, Default, Deserialize)]
struct View {
    completed_at: Option<DateTime<Utc>>,
    current_status: Option<Status>,
    current_status_update: Option<StatusUpdate>,
    #[serde(flatten)]
    project: Project,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Counts {
    num_tasks: Option<u64>,
    num_incomplete_tasks: Option<u64>,
    num_completed_tasks: Option<u64>,
}

impl View {
    fn display(&self) -> &str {
        self.project.display()
    }

    fn url(&self) -> String {
        self.project
            .permalink_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://app.asana.com/0/{}/list", self.project.gid))
    }

    fn is_archived(&self) -> bool {
        self.project.archived.unwrap_or(false)
    }

    fn is_completed(&self) -> bool {
        self.project.completed.unwrap_or(false)
    }

    /// `public`, `private to team` or `private`, from `privacy_setting`
    /// when Asana sent it, else the deprecated `public` flag; nothing when
    /// neither came back.
    fn visibility(&self) -> Option<&'static str> {
        match self.project.privacy_setting.as_deref() {
            Some("private_to_team") => Some("private to team"),
            _ => match self.project.is_public()? {
                true => Some("public"),
                false => Some("private"),
            },
        }
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
            .or_else(|| {
                self.current_status
                    .as_ref()
                    .and_then(|s| s.title.as_deref())
            })
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

/// The raw `/task_counts` object, or nothing when Asana will not say (the
/// view still prints without the numbers). `wanted` is false when `--json`
/// named a field list that leaves `task_counts` out, so the second request
/// is not made for a value nobody asked for.
async fn fetch_counts(ctx: &Ctx, gid: &str, wanted: bool) -> Option<Value> {
    if !wanted {
        return None;
    }
    ctx.api
        .get(
            &format!("/projects/{gid}/task_counts"),
            &[("opt_fields", TASK_COUNT_FIELDS)],
        )
        .await
        .ok()
}

/// Whether the run needs `/task_counts`: the human view and `--json` with
/// no field list always do; a field list does only when it asks for
/// `task_counts` (`--json task_counts.num_tasks` counts, as `json::select`
/// judges a dotted field by its first segment).
fn wants_counts(args: &Args) -> bool {
    if !args.json.is_json() {
        return true;
    }
    let fields = args.json.fields();
    fields.is_empty()
        || fields
            .iter()
            .any(|f| f.split('.').next().unwrap_or(f) == "task_counts")
}

/// The gid `input` names: a gid or URL as is, a name through the shared
/// lookup (case-insensitive, emoji-tolerant, prefix and whole-word tiers).
async fn resolve_gid(ctx: &Ctx, input: &str) -> Result<String> {
    if let Some(gid) = parse_project_gid(input) {
        return Ok(gid);
    }
    Ok(resolve_project(ctx, input).await?.gid)
}

/// The `--json` object: the project as Asana sent it with the task counts
/// folded in under `task_counts` (`null` when they could not be fetched).
fn json_object(mut raw: Value, counts: Option<Value>) -> Value {
    if let Value::Object(map) = &mut raw {
        map.insert("task_counts".to_string(), counts.unwrap_or(Value::Null));
    }
    raw
}

/// `--jq` without `--json` and unknown `--json` fields fail before the
/// token is read, as gh's do; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for Args {
    const JSON_FIELDS: &'static [&'static str] = JSON_FIELDS;

    fn prevalidate(&self, _interactive: bool) -> Result<()> {
        self.json.check(Self::JSON_FIELDS)
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if args.web {
        let raw = with_spinner(ctx, "", async {
            let gid = resolve_gid(ctx, &args.project).await?;
            Ok::<_, anyhow::Error>(fetch_view(ctx, &gid).await?)
        })
        .await?;
        let view: View = serde_json::from_value(raw)?;
        return open_in_browser(&view.url());
    }
    let wanted = wants_counts(&args);
    let (raw, counts) = with_spinner(ctx, "", async {
        let gid = resolve_gid(ctx, &args.project).await?;
        let (raw, counts) = tokio::join!(fetch_view(ctx, &gid), fetch_counts(ctx, &gid, wanted));
        Ok::<_, anyhow::Error>((raw?, counts))
    })
    .await?;
    if args.json.is_json() {
        return json::print(&json_object(raw, counts), &args.json, JSON_FIELDS);
    }
    let view: View = serde_json::from_value(raw)?;
    let counts: Counts = counts
        .and_then(|c| serde_json::from_value(c).ok())
        .unwrap_or_default();

    if ctx.is_tty {
        print!(
            "{}",
            render_tty(&view, &counts, ctx.width, ctx.color, now())
        );
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

    // `gh issue view` heads its view with the bold subject and then, plain,
    // the identifier another command would take (`... cli/cli#13111`); ours
    // is the gid, so a name lookup shows what it resolved to and leaves
    // something to paste. Nothing to add when the name IS the gid.
    let gid = &v.project.gid;
    if v.display() == gid {
        line(&mut out, &p.bold(gid));
    } else {
        line(&mut out, &format!("{} {gid}", p.bold(v.display())));
    }

    let mut meta: Vec<String> = Vec::new();
    let (label, hue) = state(v);
    meta.push(paint(&p, hue, label));
    if let Some(visibility) = v.visibility() {
        meta.push(visibility.to_string());
    }
    if let Some(owner) = v.project.owner.as_ref() {
        meta.push(format!("owned by {}", owner.display()));
    }
    if v.is_completed() {
        if let Some(t) = v.completed_at {
            meta.push(format!("completed {}", fuzzy_ago(now, t)));
        }
    } else if let Some(t) = v.project.modified_at {
        meta.push(format!("updated {}", fuzzy_ago(now, t)));
    } else if let Some(t) = v.project.created_at {
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
    if let Some(due) = v.project.due_on {
        line(
            &mut out,
            &format!("{}{}", p.bold("Due: "), due_line(v, due, &p)),
        );
    }
    if let Some(team) = v.project.team.as_ref() {
        line(&mut out, &format!("{}{}", p.bold("Team: "), team.display()));
    }
    if !v.project.members.is_empty() {
        line(
            &mut out,
            &format!("{}{}", p.bold("Members: "), member_list(&v.project.members)),
        );
    }

    // Body: blank, blank, the notes in gh's issue-body shape, blank, blank.
    // Nothing to show is one muted line with a single blank line each
    // side, the way `gh repo view` prints its no-README line.
    let body = body_lines(
        v.project.html_notes.as_deref(),
        v.project.notes.as_deref(),
        width,
        &p,
    );
    if body.is_empty() {
        out.push('\n');
        line(
            &mut out,
            &format!("  {}", p.muted("No description provided")),
        );
        out.push('\n');
    } else {
        out.push('\n');
        out.push('\n');
        for l in body {
            line(&mut out, &l);
        }
        out.push('\n');
        out.push('\n');
    }
    line(
        &mut out,
        &p.muted(&format!("View this project in Asana: {}", v.url())),
    );
    out
}

/// The description, in `gh`'s issue-body shape: wrapped to `width - 4`
/// under a two-space margin, so the text keeps two free columns on the
/// right the way glamour's document margin does.
///
/// `render::notes::render_body` also pads every line out to `width - 2`,
/// because glamour fills the line it has painted. Nothing here paints a
/// background behind running text, so that fill is nothing but trailing
/// whitespace on every line of the description, which a copy-paste carries
/// away with it. Same layout, `pad_to: None`.
fn body_lines(
    html: Option<&str>,
    text: Option<&str>,
    width: usize,
    palette: &Palette,
) -> Vec<String> {
    render_lines(
        html,
        text,
        &Layout {
            width: width.saturating_sub(4).max(20),
            color: palette.enabled,
            margin: 2,
            pad_to: None,
        },
    )
}

/// `tomorrow (Sep 11)`, `3 days overdue (Sep 7)` in red, `today` in yellow;
/// a finished project (complete or archived) gets the plain date, since
/// nothing about it is overdue any more.
fn due_line(v: &View, due: chrono::NaiveDate, p: &Palette) -> String {
    let today = today();
    let full = short_date(due, today);
    if v.is_completed() || v.is_archived() {
        return full;
    }
    let (phrase, due_state) = due_phrase(due, today);
    let painted = match due_state {
        DueState::Overdue => p.red(&phrase),
        DueState::Today => p.yellow(&phrase),
        DueState::Upcoming => phrase.clone(),
    };
    if full == phrase {
        painted
    } else {
        format!("{painted} ({full})")
    }
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
/// Every key is printed even when empty so the line count is stable.
fn render_piped(v: &View, counts: &Counts) -> String {
    let mut out = String::new();
    let mut kv = |key: &str, value: String| {
        out.push_str(key);
        out.push_str(":\t");
        out.push_str(&value);
        out.push('\n');
    };
    let count = |n: Option<u64>| n.map(|n| n.to_string()).unwrap_or_default();
    kv("title", v.display().to_string());
    kv("state", state(v).0.to_uppercase());
    kv("visibility", v.visibility().unwrap_or("").to_string());
    kv(
        "owner",
        v.project
            .owner
            .as_ref()
            .map(|o| o.display())
            .unwrap_or("")
            .to_string(),
    );
    kv(
        "team",
        v.project
            .team
            .as_ref()
            .map(|t| t.display())
            .unwrap_or("")
            .to_string(),
    );
    kv(
        "members",
        v.project
            .members
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
    kv(
        "due",
        v.project.due_on.map(|d| d.to_string()).unwrap_or_default(),
    );
    kv("tasks", count(counts.num_tasks));
    kv("open", count(counts.num_incomplete_tasks));
    kv("completed", count(counts.num_completed_tasks));
    kv("created", v.project.created_at.map(iso).unwrap_or_default());
    kv(
        "updated",
        v.project.modified_at.map(iso).unwrap_or_default(),
    );
    kv("gid", v.project.gid.clone());
    kv("url", v.url());
    out.push_str("--\n");
    out.push_str(v.project.notes.as_deref().map(str::trim_end).unwrap_or(""));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use unicode_width::UnicodeWidthStr;

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
            "public": false,
            "privacy_setting": "private_to_team",
            "current_status": {"title": "Status Update - Sep 5", "color": "green", "created_at": "2026-09-05T09:00:00.000Z"},
            "current_status_update": {"status_type": "on_track", "title": "Status Update - Sep 5", "created_at": "2026-09-05T09:00:00.000Z"}
        }))
        .expect("sample project")
    }

    /// The sample with a description long enough to wrap at 80 columns.
    fn long_notes() -> View {
        let mut v = sample();
        let text = "The quarter's plan, written out at length so the renderer has to wrap it more than once at eighty columns.";
        v.project.html_notes = Some(format!("<body>{text}</body>"));
        v.project.notes = Some(text.to_string());
        v
    }

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 9, 10, 0, 0)
            .single()
            .expect("time")
    }

    #[test]
    fn flattened_project_keeps_every_field() {
        let v = sample();
        assert_eq!(v.project.gid, "1200000000000000");
        assert_eq!(v.display(), "Roadmap");
        assert_eq!(v.project.members.len(), 2);
        assert_eq!(
            v.project.due_on.map(|d| d.to_string()).as_deref(),
            Some("2026-09-30")
        );
        assert_eq!(v.visibility(), Some("private to team"));
        assert!(v.current_status.as_ref().unwrap().created_at.is_some());
        assert_eq!(v.status_title(), Some("Status Update - Sep 5"));
        let bare: View = serde_json::from_value(serde_json::json!({"gid": "7"})).unwrap();
        assert_eq!(bare.display(), "7");
        assert_eq!(bare.visibility(), None);
        assert_eq!(bare.url(), "https://app.asana.com/0/7/list");
    }

    #[test]
    fn visibility_prefers_privacy_setting() {
        let mut v = sample();
        v.project.privacy_setting = Some("public_to_workspace".into());
        assert_eq!(v.visibility(), Some("public"));
        v.project.privacy_setting = Some("private".into());
        assert_eq!(v.visibility(), Some("private"));
        v.project.privacy_setting = None;
        v.project.public = Some(true);
        assert_eq!(v.visibility(), Some("public"));
        v.project.public = Some(false);
        assert_eq!(v.visibility(), Some("private"));
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
        assert_eq!(lines[0], "Roadmap 1200000000000000");
        assert_eq!(
            lines[1],
            "Active • private to team • owned by Div • updated about 2 hours ago • 10 of 12 tasks open"
        );
        assert_eq!(
            lines[2],
            "Status: On track — Status Update - Sep 5 (about 4 days ago)"
        );
        assert!(lines[3].starts_with("Due: "), "{}", lines[3]);
        assert_eq!(lines[4], "Team: Engineering");
        assert_eq!(lines[5], "Members: Div, Alice");
        assert_eq!(lines[6], "");
        assert_eq!(lines[7], "");
        // The body keeps gh's two-space margin and its two free columns on
        // the right, but carries no trailing whitespace.
        assert_eq!(lines[8], "  Plan for Q4.");
        assert_eq!(lines[9], "  Ship the CLI.");
        for l in &lines {
            assert_eq!(l.trim_end(), *l, "trailing whitespace in {l:?}");
        }
        let long = render_tty(&long_notes(), &counts, 80, false, at());
        for l in long.lines().filter(|l| l.starts_with("  ")) {
            assert!(UnicodeWidthStr::width(l) <= 78, "{l:?}");
            assert_eq!(l.trim_end(), l, "trailing whitespace in {l:?}");
        }
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
            project: Project {
                gid: "42".into(),
                name: Some("Bare".into()),
                ..Project::default()
            },
            ..View::default()
        };
        let out = render_tty(&v, &Counts::default(), 80, false, at());
        assert_eq!(
            out,
            "Bare 42\nActive\n\n  No description provided\n\nView this project in Asana: https://app.asana.com/0/42/list\n"
        );
    }

    /// A project Asana sent no name for displays as its gid; the header
    /// does not then print it twice.
    #[test]
    fn a_nameless_project_shows_its_gid_once() {
        let v: View = serde_json::from_value(serde_json::json!({"gid": "42"})).unwrap();
        let out = render_tty(&v, &Counts::default(), 80, false, at());
        assert_eq!(out.lines().next(), Some("42"));
    }

    #[test]
    fn emoji_names_survive_the_header() {
        let v: View = serde_json::from_value(serde_json::json!({
            "gid": "9",
            "name": "👨\u{200d}⚖️ LEGAL",
            "html_notes": "<body></body>",
            "notes": ""
        }))
        .unwrap();
        let out = render_tty(&v, &Counts::default(), 120, true, at());
        assert!(
            out.starts_with("\x1b[0;1;39m👨\u{200d}⚖️ LEGAL\x1b[0m 9\n"),
            "{out:?}"
        );
        assert_eq!(
            render_piped(&v, &Counts::default()).lines().next(),
            Some("title:\t👨\u{200d}⚖️ LEGAL")
        );
    }

    #[test]
    fn colour_placement() {
        let counts = Counts::default();
        let out = render_tty(&sample(), &counts, 80, true, at());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "\x1b[0;1;39mRoadmap\x1b[0m 1200000000000000");
        assert!(
            lines[1].starts_with("\x1b[0;32mActive\x1b[0m • private to team • owned by Div"),
            "{:?}",
            lines[1]
        );
        assert!(
            lines[2].starts_with("\x1b[0;1;39mStatus: \x1b[0m\x1b[0;32mOn track\x1b[0m"),
            "{:?}",
            lines[2]
        );
        assert!(lines[4].starts_with("\x1b[0;1;39mTeam: \x1b[0mEngineering"));
        assert!(lines
            .last()
            .unwrap()
            .starts_with("\x1b[38;5;242mView this project in Asana:"));
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
            "title:\tRoadmap\nstate:\tACTIVE\nvisibility:\tprivate to team\nowner:\tDiv\nteam:\tEngineering\nmembers:\tDiv, Alice\nstatus:\tOn track\ndue:\t2026-09-30\ntasks:\t12\nopen:\t10\ncompleted:\t2\ncreated:\t2026-06-01T10:00:00Z\nupdated:\t2026-09-09T08:00:00Z\ngid:\t1200000000000000\nurl:\thttps://app.asana.com/0/1200000000000000/1200000000000000\n--\nPlan for Q4.\nShip the CLI.\n"
        );
        assert!(!out.contains("\x1b["));
    }

    #[test]
    fn piped_empty_notes_still_end_with_a_line() {
        let v: View = serde_json::from_value(serde_json::json!({"gid": "7", "notes": ""})).unwrap();
        let out = render_piped(&v, &Counts::default());
        assert!(
            out.ends_with("url:\thttps://app.asana.com/0/7/list\n--\n\n"),
            "{out:?}"
        );
        assert_eq!(out.lines().count(), 17);
    }

    #[test]
    fn finished_projects_are_never_overdue() {
        let mut v = sample();
        let p = Palette::new(true);
        let past = chrono::NaiveDate::from_ymd_opt(2000, 4, 30).unwrap();
        assert!(due_line(&v, past, &p).contains("overdue"));
        assert!(due_line(&v, past, &p).contains("\x1b[0;31m"));
        v.project.completed = Some(true);
        assert_eq!(due_line(&v, past, &p), "Apr 30, 2000");
        v.project.completed = None;
        v.project.archived = Some(true);
        assert_eq!(due_line(&v, past, &p), "Apr 30, 2000");
    }

    #[test]
    fn states_and_summaries() {
        let mut v = sample();
        assert_eq!(state(&v).0, "Active");
        v.project.completed = Some(true);
        assert_eq!(state(&v).0, "Complete");
        v.project.archived = Some(true);
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

    /// What `GET /projects/{gid}?opt_fields=VIEW_FIELDS` sends back: one
    /// top-level key per requested field (plus `gid`), so the JSON FIELDS
    /// list can be checked against the request rather than a guess.
    #[test]
    fn json_fields_cover_every_key_the_view_asks_for() {
        use crate::cmd::Prevalidate;
        let mut keys: Vec<&str> = VIEW_FIELDS
            .split(',')
            .map(|f| f.split('.').next().unwrap_or(f))
            .collect();
        keys.push("gid");
        keys.push("task_counts");
        keys.sort_unstable();
        keys.dedup();
        let mut declared: Vec<&str> = JSON_FIELDS.to_vec();
        declared.sort_unstable();
        assert_eq!(declared, keys);
        assert_eq!(<Args as Prevalidate>::JSON_FIELDS, JSON_FIELDS);
        // The fallback field list is a subset, so nothing there is unknown.
        for f in PROJECT_FIELDS.split(',') {
            let head = f.split('.').next().unwrap_or(f);
            assert!(JSON_FIELDS.contains(&head), "{head} not in JSON_FIELDS");
        }
    }

    #[test]
    fn json_object_folds_the_task_counts_in() {
        let raw = serde_json::json!({"gid": "1", "name": "Roadmap"});
        let counts = serde_json::json!({"num_tasks": 12, "num_incomplete_tasks": 10, "num_completed_tasks": 2});
        assert_eq!(
            json_object(raw.clone(), Some(counts.clone())),
            serde_json::json!({"gid": "1", "name": "Roadmap", "task_counts": counts})
        );
        assert_eq!(
            json_object(raw, None),
            serde_json::json!({"gid": "1", "name": "Roadmap", "task_counts": null})
        );
        // Anything that is not an object is left alone.
        assert_eq!(json_object(Value::Null, None), Value::Null);
    }

    #[derive(clap::Parser, Debug)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        use clap::Parser;
        Cli::try_parse_from(std::iter::once("asana").chain(argv.iter().copied())).map(|c| c.args)
    }

    #[test]
    fn json_flags_parse_and_prevalidate_like_gh() {
        use crate::cmd::Prevalidate;
        let plain = parse(&["Roadmap"]).unwrap();
        assert_eq!(plain.project, "Roadmap");
        assert!(!plain.json.is_json());
        assert!(plain.prevalidate(true).is_ok());

        let all = parse(&["Roadmap", "--json"]).unwrap();
        assert!(all.json.is_json());
        assert!(all.json.fields().is_empty());
        assert!(all.prevalidate(true).is_ok());

        let some = parse(&["Roadmap", "--json", "name,owner.name,task_counts"]).unwrap();
        assert_eq!(
            some.json.fields(),
            vec!["name", "owner.name", "task_counts"]
        );
        assert!(some.prevalidate(true).is_ok());

        let jq = parse(&[
            "1200000000000000",
            "--json",
            "permalink_url",
            "--jq",
            ".permalink_url",
        ])
        .unwrap();
        assert_eq!(jq.json.jq.as_deref(), Some(".permalink_url"));
        assert!(jq.prevalidate(true).is_ok());

        // `--json` before the argument would swallow it as the field list,
        // so a following `--web` is not taken as one either.
        let web = parse(&["Roadmap", "--json", "--web"]).unwrap();
        assert!(web.json.is_json() && web.web);

        let lone = parse(&["Roadmap", "--jq", "."]).unwrap();
        assert_eq!(
            lone.prevalidate(true).unwrap_err().to_string(),
            "cannot use `--jq` without specifying `--json`"
        );
        let bad = parse(&["Roadmap", "--json", "nope"]).unwrap();
        let err = bad.prevalidate(true).unwrap_err().to_string();
        assert!(
            err.starts_with("Unknown JSON field: \"nope\"\nAvailable fields:\n  archived\n"),
            "{err}"
        );
        assert!(err.ends_with("\n  workspace"), "{err}");
    }

    /// The second request is made for the human view and for any `--json`
    /// that can print the counts, and skipped for a field list that cannot.
    #[test]
    fn the_counts_request_is_made_only_when_it_can_be_printed() {
        for argv in [
            &["Roadmap"][..],
            &["Roadmap", "--web"],
            &["Roadmap", "--json"],
            &["Roadmap", "--json", "--jq", ".task_counts"],
            &["Roadmap", "--json", "name,task_counts"],
            &["Roadmap", "--json", "task_counts.num_tasks"],
        ] {
            assert!(wants_counts(&parse(argv).unwrap()), "{argv:?}");
        }
        for argv in [
            &["Roadmap", "--json", "name"][..],
            &["Roadmap", "--json", "gid,permalink_url"],
            &["Roadmap", "--json", "owner", "--jq", ".owner.name"],
        ] {
            assert!(!wants_counts(&parse(argv).unwrap()), "{argv:?}");
        }
    }

    #[test]
    fn json_output_selects_fields_and_runs_jq() {
        let raw = serde_json::json!({
            "gid": "1200000000000000",
            "name": "Roadmap",
            "owner": {"gid": "1", "name": "Div"},
            "permalink_url": "https://app.asana.com/0/1200000000000000/list"
        });
        let counts = serde_json::json!({"num_tasks": 3, "num_incomplete_tasks": 1, "num_completed_tasks": 2});
        let value = json_object(raw, Some(counts));

        let args = parse(&["Roadmap", "--json", "name,owner.name"]).unwrap();
        assert_eq!(
            json::render(&value, &args.json, JSON_FIELDS).unwrap(),
            "{\n  \"name\": \"Roadmap\",\n  \"owner\": {\n    \"name\": \"Div\"\n  }\n}\n"
        );
        let args = parse(&[
            "Roadmap",
            "--json",
            "--jq",
            ".task_counts.num_incomplete_tasks",
        ])
        .unwrap();
        assert_eq!(
            json::render(&value, &args.json, JSON_FIELDS).unwrap(),
            "1\n"
        );
        let args = parse(&[
            "Roadmap",
            "--json",
            "permalink_url",
            "--jq",
            ".permalink_url",
        ])
        .unwrap();
        assert_eq!(
            json::render(&value, &args.json, JSON_FIELDS).unwrap(),
            "https://app.asana.com/0/1200000000000000/list\n"
        );
    }
}
