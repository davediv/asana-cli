//! `asana task list`: tasks assigned to you, or the tasks of a project.
//!
//! Modelled on `gh issue list` (see `reference/gh/issue-list*.txt`): a blank
//! line, a `Showing N of M ...` summary, a blank line, then a table with a
//! dim underlined header, a green (open) or magenta (completed) identifier
//! column, and a muted relative timestamp. Piped, it is tab-separated with no
//! header and ISO-8601 dates; `--json [fields]` is the raw API array, shaped
//! by the field list and `--jq` like gh's.
//!
//! Tasks come in the order Asana shows them (your My Tasks list, or the
//! project's own order), so only `--limit` of them are fetched: the API has
//! no sort of its own, and reading a whole My Tasks list to sort it took
//! seven requests for a five-row table. A project's total comes from
//! `/projects/P/task_counts`; a My Tasks list has no counter, so its total
//! reads `100+` once a full page of a hundred has come back.

use std::fmt;

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde_json::Value;

use crate::api::{
    ApiError, Named, Project, Task, TaskCounts, Workspace, TASK_COUNT_FIELDS, TASK_FIELDS,
};
use crate::cli::usage_error;
use crate::cmd::{open_in_browser, with_spinner};
use crate::ctx::Ctx;
use crate::render::json::{self, JsonArgs};
use crate::render::table::Cell;
use crate::render::time::{due_phrase, fuzzy_ago, iso, now, short_date, today, DueState};
use crate::render::{Palette, Table};
use crate::resolve::{resolve_assignee_value, resolve_project};

/// The command's path, for usage errors.
const PATH: &[&str] = &["task", "list"];

/// Every top-level key of the objects `--json` prints (what `TASK_FIELDS`
/// asks the API for, plus `gid`), for `--json <fields>` and the JSON FIELDS
/// section of `--help`.
const JSON_FIELDS: &[&str] = &[
    "gid",
    "name",
    "notes",
    "completed",
    "completed_at",
    "assignee",
    "due_on",
    "due_at",
    "start_on",
    "created_at",
    "modified_at",
    "projects",
    "memberships",
    "tags",
    "parent",
    "num_subtasks",
    "permalink_url",
    "created_by",
    "followers",
    "num_likes",
    "custom_fields",
    "workspace",
];

/// How many tasks to read (at least) before calling a My Tasks list
/// `100+`: one page, the most the API sends per request.
const PROBE: usize = 100;

/// `--state`: which tasks to show. `closed` is accepted quietly for people
/// with `gh` muscle memory.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    #[value(alias = "closed")]
    Completed,
    All,
}

#[derive(clap::Args, Debug)]
#[command(
    about = "List tasks assigned to you or in a project",
    long_about = "List tasks in an Asana workspace. By default, this only lists open tasks\n\
        assigned to you, in the order of your My Tasks list.\n\n\
        Use `--project` to list the tasks of a project, in the project's own order,\n\
        instead. A project can be given by gid, by URL, or by name\n\
        (case-insensitive; a unique prefix is enough).\n\n\
        When stdout is not a terminal the output is tab-separated with no header,\n\
        one task per line, with ISO-8601 dates.\n\n\
        For more information about output formatting flags, see `asana help formatting`.",
    after_help = "$ asana task list\n\
        $ asana task list --project \"Roadmap\"\n\
        $ asana task list --assignee \"jane@example.com\"\n\
        $ asana task list --assignee \"@me\" --state all\n\
        $ asana task list --state completed --limit 50\n\
        $ asana task list --project Roadmap --json gid,name,due_on\n\
        $ asana task list --json name,permalink_url --jq \".[].permalink_url\""
)]
pub struct Args {
    /// List the tasks of a project (a gid, a URL or a name)
    #[arg(short = 'p', long, value_name = "string")]
    pub project: Option<String>,

    /// Filter by assignee (@me, an email, a name or a gid)
    #[arg(short = 'a', long, value_name = "user")]
    pub assignee: Option<String>,

    /// Filter by state: {open|completed|all}
    #[arg(short = 's', long, value_name = "string", value_enum, default_value_t = State::Open)]
    pub state: State,

    /// Maximum number of tasks to fetch
    #[arg(short = 'L', long, value_name = "int", default_value_t = 30)]
    pub limit: usize,

    #[command(flatten)]
    pub json: JsonArgs,

    /// List tasks in the web browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

/// A column of the terminal table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Id,
    /// `open` / `completed`, only when the listing can hold both.
    State,
    Title,
    /// The task's projects when listing your tasks, its assignee when
    /// listing a project.
    Context,
    Due,
    Updated,
}

/// Widths the layout budget assumes: a gid, the widest state, the least a
/// title should keep, a project or assignee name, `163 days overdue`, and
/// `less than a minute ago`.
const ID_WIDTH: usize = 16;
const STATE_WIDTH: usize = 9;
const MIN_TITLE_WIDTH: usize = 20;
const CONTEXT_WIDTH: usize = 18;
const DUE_WIDTH: usize = 16;
const UPDATED_WIDTH: usize = 22;
const GUTTER: usize = 2;

/// Which columns a terminal of `width` gets. gh never drops columns, but its
/// IDs are six characters; a 16-digit gid would starve the title on a
/// narrow terminal, so the middle columns give way (CONTEXT first, then DUE,
/// then STATE) and the title keeps at least twenty cells. Piped output
/// always carries every column, STATE included.
fn columns(is_tty: bool, width: usize, show_state: bool) -> Vec<Col> {
    if !is_tty {
        return vec![
            Col::Id,
            Col::State,
            Col::Title,
            Col::Context,
            Col::Due,
            Col::Updated,
        ];
    }
    let mut cols = vec![Col::Id, Col::Title, Col::Updated];
    let mut used = ID_WIDTH + MIN_TITLE_WIDTH + UPDATED_WIDTH + GUTTER * 2;
    let mut wanted: Vec<(Col, usize)> = Vec::new();
    if show_state {
        wanted.push((Col::State, STATE_WIDTH));
    }
    wanted.push((Col::Due, DUE_WIDTH));
    wanted.push((Col::Context, CONTEXT_WIDTH));
    for (col, w) in wanted {
        if used + w + GUTTER <= width {
            used += w + GUTTER;
            cols.push(col);
        }
    }
    // Display order, whatever the order they were admitted in.
    let order = [
        Col::Id,
        Col::State,
        Col::Title,
        Col::Context,
        Col::Due,
        Col::Updated,
    ];
    order.into_iter().filter(|c| cols.contains(c)).collect()
}

/// How many tasks there are: exact, or at least this many when the listing
/// stopped reading before the end (`100+` in the summary line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Total {
    Exact(usize),
    AtLeast(usize),
}

impl fmt::Display for Total {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Total::Exact(n) => write!(f, "{n}"),
            Total::AtLeast(n) => write!(f, "{n}+"),
        }
    }
}

/// gh's `text.Pluralize`: `1 open task`, `2 open tasks`, `100+ open tasks`.
fn plural(total: Total, word: &str) -> String {
    match total {
        Total::Exact(1) => format!("1 {word}"),
        other => format!("{other} {word}s"),
    }
}

/// gh's `ListHeader`: the line above the table. `scope` already carries its
/// preposition: `in Roadmap`, `assigned to you in Acme`.
fn summary_line(shown: usize, total: Total, scope: &str, has_filters: bool) -> String {
    if has_filters {
        let verb = if total == Total::Exact(1) {
            "matches"
        } else {
            "match"
        };
        format!(
            "Showing {shown} of {} {scope} that {verb} your search",
            plural(total, "task")
        )
    } else {
        format!("Showing {shown} of {} {scope}", plural(total, "open task"))
    }
}

/// gh's `ListNoResults`: the one line printed when nothing matched.
fn empty_line(scope: &str, has_filters: bool) -> String {
    if has_filters {
        format!("no tasks match your search {scope}")
    } else {
        format!("no open tasks {scope}")
    }
}

/// Table cells must stay on one line and, when piped, must not contain tabs.
fn one_line(s: &str) -> String {
    s.split(['\n', '\r', '\t'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The third column: the task's projects when listing your tasks, its
/// assignee when listing a project.
fn context_column(task: &Task, listing_project: bool) -> String {
    if listing_project {
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
    }
}

/// The due date as a `NaiveDate` in local time, if the task has one.
fn due_date(task: &Task) -> Option<chrono::NaiveDate> {
    task.due_on.or_else(|| {
        task.due_at
            .map(|t| t.with_timezone(&chrono::Local).date_naive())
    })
}

/// Whether `state` keeps a task.
fn wanted(state: State, task: &Task) -> bool {
    match state {
        State::Open => !task.is_completed(),
        State::Completed => task.is_completed(),
        State::All => true,
    }
}

/// Everything the API gave us, already filtered and counted.
struct Listing {
    items: Vec<(Value, Task)>,
    total: Total,
    project: Option<Project>,
    workspace: Option<Workspace>,
}

impl Listing {
    /// Where the tasks were looked for, with its preposition: `in Roadmap`,
    /// `in Acme`, or for the unfiltered default `assigned to you in Acme`
    /// (gh's `in cli/cli`).
    fn scope(&self, unfiltered: bool) -> String {
        match (&self.project, &self.workspace) {
            (Some(p), _) => format!("in {}", p.display()),
            (None, Some(ws)) if unfiltered => format!("assigned to you in {}", ws.display()),
            (None, Some(ws)) => format!("in {}", ws.display()),
            (None, None) => "in the workspace".to_string(),
        }
    }
}

/// Decode the raw task objects, keeping the raw value for `--json`.
fn decode(raw: Vec<Value>) -> Vec<(Value, Task)> {
    raw.into_iter()
        .filter_map(|v| {
            serde_json::from_value::<Task>(v.clone())
                .ok()
                .map(|t| (v, t))
        })
        .collect()
}

/// The gid of `user`'s My Tasks list in `workspace` (`me` or a user gid):
/// `None` when Asana will not show it to us, so the caller can fall back
/// to the plain `/tasks?assignee=` query.
async fn user_task_list(ctx: &Ctx, user: &str, workspace: &str) -> Result<Option<String>> {
    let list: Result<Named, ApiError> = ctx
        .api
        .get(
            &format!("/users/{user}/user_task_list"),
            &[("workspace", workspace), ("opt_fields", "name")],
        )
        .await;
    match list {
        Ok(list) if !list.gid.is_empty() => Ok(Some(list.gid)),
        Ok(_) => Ok(None),
        Err(ApiError::Forbidden { .. } | ApiError::NotFound { .. } | ApiError::Status { .. }) => {
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// The tasks of a project, in the project's own order, with the total from
/// `/projects/P/task_counts` (one cheap request, alongside the page) when
/// the API can filter for us, else read to the end and counted here.
async fn fetch_project(
    ctx: &Ctx,
    project: &Project,
    state: State,
    assignee: Option<&str>,
    limit: usize,
    want_total: bool,
) -> Result<(Vec<(Value, Task)>, Total)> {
    let mut query: Vec<(&str, &str)> = vec![("opt_fields", TASK_FIELDS), ("project", &project.gid)];
    if state == State::Open {
        // Incomplete tasks plus those completed since now, i.e. none.
        query.push(("completed_since", "now"));
    }
    let api_filters = state != State::Completed && assignee.is_none();
    if !api_filters {
        // Completed-only, or an assignee: the API cannot combine these with
        // `project=`, so read everything and filter here.
        let raw: Vec<Value> = ctx.api.get_all("/tasks", &query, None).await?;
        let mut items = decode(raw);
        let wanted_gid = match assignee {
            Some("me") => Some(ctx.me().await?.gid),
            Some(gid) => Some(gid.to_string()),
            None => None,
        };
        items.retain(|(_, t)| {
            wanted(state, t)
                && wanted_gid
                    .as_deref()
                    .is_none_or(|g| t.assignee.as_ref().is_some_and(|a| a.gid == g))
        });
        let total = Total::Exact(items.len());
        items.truncate(limit);
        return Ok((items, total));
    }
    if !want_total {
        let raw: Vec<Value> = ctx.api.get_all("/tasks", &query, Some(limit)).await?;
        let items = decode(raw);
        let total = Total::Exact(items.len());
        return Ok((items, total));
    }
    let counts_path = format!("/projects/{}/task_counts", project.gid);
    let (raw, counts): (Result<Vec<Value>, ApiError>, Result<TaskCounts, ApiError>) = tokio::join!(
        ctx.api.get_all("/tasks", &query, Some(limit)),
        ctx.api
            .get(&counts_path, &[("opt_fields", TASK_COUNT_FIELDS)]),
    );
    let items = decode(raw?);
    let counted = counts.ok().and_then(|c| match state {
        State::Open => c.num_incomplete_tasks,
        State::All => c.num_tasks,
        State::Completed => c.num_completed_tasks,
    });
    let total = match counted {
        // A page can be a beat ahead of the counter; never claim fewer
        // than we hold.
        Some(n) => Total::Exact((n as usize).max(items.len())),
        None if items.len() >= limit => Total::AtLeast(limit),
        None => Total::Exact(items.len()),
    };
    Ok((items, total))
}

/// The tasks assigned to `user` (`me` or a gid) in `workspace`, in the
/// order of their My Tasks list (or, when Asana hides that list, in the
/// order `/tasks?assignee=` returns). Open and all-state listings stop
/// after `limit` rows (a whole page when a total is wanted, so the
/// summary can say `100+`); a completed-only listing must read to the
/// end, since the API cannot filter for it.
async fn fetch_assigned(
    ctx: &Ctx,
    user: &str,
    workspace: &Workspace,
    state: State,
    limit: usize,
    want_total: bool,
) -> Result<(Vec<(Value, Task)>, Total)> {
    let list = user_task_list(ctx, user, &workspace.gid).await?;
    let path = match &list {
        Some(gid) => format!("/user_task_lists/{gid}/tasks"),
        None => "/tasks".to_string(),
    };
    let mut query: Vec<(&str, &str)> = vec![("opt_fields", TASK_FIELDS)];
    if list.is_none() {
        query.push(("assignee", user));
        query.push(("workspace", &workspace.gid));
    }
    if state == State::Open {
        query.push(("completed_since", "now"));
    }
    if state == State::Completed {
        let raw: Vec<Value> = ctx.api.get_all(&path, &query, None).await?;
        let mut items = decode(raw);
        items.retain(|(_, t)| wanted(state, t));
        let total = Total::Exact(items.len());
        items.truncate(limit);
        return Ok((items, total));
    }
    let probe = if want_total { limit.max(PROBE) } else { limit };
    let raw: Vec<Value> = ctx.api.get_all(&path, &query, Some(probe)).await?;
    let mut items = decode(raw);
    let total = if want_total && items.len() >= probe {
        Total::AtLeast(probe)
    } else {
        Total::Exact(items.len())
    };
    items.truncate(limit);
    Ok((items, total))
}

/// Resolve identifiers and fetch the tasks. Separated from `run` so the
/// spinner can be stopped before anything is printed, success or error.
/// `want_total` is whether a `Showing N of M` line will be printed.
async fn fetch(ctx: &Ctx, args: &Args, want_total: bool) -> Result<Listing> {
    let project = match &args.project {
        Some(p) => Some(resolve_project(ctx, p).await?),
        None => None,
    };
    // Listing your own tasks needs the workspace; a project already implies it.
    let workspace = match &project {
        Some(_) => None,
        None => Some(ctx.workspace().await?),
    };
    // Who are we filtering by? Default: you, unless a project is given.
    // `me` is sent as is (Asana accepts it), so it costs no request.
    let assignee: Option<String> = match (&args.assignee, &project) {
        (Some(a), _) => Some(resolve_assignee_value(ctx, a).await?),
        (None, None) => Some("me".to_string()),
        (None, Some(_)) => None,
    };

    let (items, total) = match (&project, &workspace) {
        (Some(p), _) => {
            fetch_project(
                ctx,
                p,
                args.state,
                assignee.as_deref(),
                args.limit,
                want_total,
            )
            .await?
        }
        (None, Some(ws)) => {
            let user = assignee.as_deref().unwrap_or("me");
            fetch_assigned(ctx, user, ws, args.state, args.limit, want_total).await?
        }
        (None, None) => (Vec::new(), Total::Exact(0)),
    };

    Ok(Listing {
        items,
        total,
        project,
        workspace,
    })
}

/// Where `--web` goes: the project's list view, or your My Tasks list.
async fn web_url(ctx: &Ctx, args: &Args) -> Result<String> {
    if let Some(p) = &args.project {
        let project = resolve_project(ctx, p).await?;
        return Ok(project
            .permalink_url
            .unwrap_or_else(|| format!("https://app.asana.com/0/{}/list", project.gid)));
    }
    let ws = ctx.workspace().await?;
    Ok(match user_task_list(ctx, "me", &ws.gid).await? {
        Some(list) => format!("https://app.asana.com/0/{list}/list"),
        None => format!("https://app.asana.com/0/home/{}", ws.gid),
    })
}

/// gh's own checks, before the token is read: `invalid limit: 0` (a flag
/// error, so the usage block follows it), `--jq` without `--json`, and an
/// unknown `--json` field.
impl crate::cmd::Prevalidate for Args {
    const JSON_FIELDS: &'static [&'static str] = JSON_FIELDS;

    fn prevalidate(&self, _interactive: bool) -> Result<()> {
        if self.limit < 1 {
            return Err(usage_error(PATH, format!("invalid limit: {}", self.limit)));
        }
        self.json.check(Self::JSON_FIELDS)
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if args.web {
        let url = with_spinner(ctx, "", web_url(ctx, &args)).await?;
        return open_in_browser(&url);
    }

    // Only a terminal table carries a `Showing N of M` line.
    let want_total = ctx.is_tty && !args.json.is_json();
    let listing = with_spinner(ctx, "", fetch(ctx, &args, want_total)).await?;

    if args.json.is_json() {
        let values: Vec<Value> = listing.items.into_iter().map(|(v, _)| v).collect();
        return json::print(&Value::Array(values), &args.json, JSON_FIELDS);
    }

    // Like gh, `--state` and `--assignee` are searches; `--project` is a scope.
    let has_filters = args.state != State::Open || args.assignee.is_some();
    let scope = listing.scope(!has_filters);

    if listing.items.is_empty() {
        // gh prints this on stderr, and only when stdout is a terminal.
        if ctx.is_tty {
            eprintln!("{}", empty_line(&scope, has_filters));
        }
        return Ok(());
    }

    if ctx.is_tty {
        println!();
        println!(
            "{}",
            summary_line(listing.items.len(), listing.total, &scope, has_filters)
        );
        println!();
    }

    print!(
        "{}",
        render_table(
            ctx.is_tty,
            ctx.width,
            ctx.color,
            &listing,
            args.state,
            now(),
            today()
        )
    );
    Ok(())
}

/// Build the table for the listing. `now`/`today` are passed in so tests
/// can pin the relative phrases. A STATE column appears when `state` lets
/// completed tasks in, so open and completed rows read apart without colour.
fn render_table(
    is_tty: bool,
    width: usize,
    color: bool,
    listing: &Listing,
    state: State,
    now: DateTime<Utc>,
    today: chrono::NaiveDate,
) -> String {
    let listing_project = listing.project.is_some();
    let cols = columns(is_tty, width, state != State::Open);
    let headers: Vec<&str> = cols
        .iter()
        .map(|c| match c {
            Col::Id => "ID",
            Col::State => "STATE",
            Col::Title => "TITLE",
            Col::Context if listing_project => "ASSIGNEE",
            Col::Context => "PROJECT",
            Col::Due => "DUE",
            Col::Updated => "UPDATED",
        })
        .collect();

    let mut table = Table::new(&headers, is_tty, width, Palette::new(color));
    for (_, task) in &listing.items {
        let completed = task.is_completed();
        let title = one_line(task.display());
        let context = one_line(&context_column(task, listing_project));
        // A completed task is not overdue: a past date shows plain (`Sep 7`)
        // while an open one reads `3 days overdue`; `today` and `tomorrow`
        // read the same for both.
        let (due, due_state) = match due_date(task) {
            Some(d) if !is_tty => (d.to_string(), DueState::Upcoming),
            Some(d) => match due_phrase(d, today) {
                (_, DueState::Overdue) if completed => (short_date(d, today), DueState::Upcoming),
                (_, DueState::Today) if completed => ("today".to_string(), DueState::Upcoming),
                phrase => phrase,
            },
            None => (String::new(), DueState::Upcoming),
        };
        let updated = match task.modified_at {
            Some(t) if is_tty => fuzzy_ago(now, t),
            Some(t) => iso(t),
            None => String::new(),
        };
        // Green for open, magenta for completed: gh's open/closed colours.
        let state_color: fn(&Palette, &str) -> String = if completed {
            |p, s| p.magenta(s)
        } else {
            |p, s| p.green(s)
        };

        let row: Vec<Cell> = cols
            .iter()
            .map(|c| match c {
                Col::Id if is_tty => Cell::fixed(task.gid.clone(), state_color),
                Col::Id => Cell::plain(task.gid.clone()),
                Col::State if is_tty => {
                    Cell::colored(if completed { "completed" } else { "open" }, state_color)
                }
                Col::State => Cell::plain(if completed { "COMPLETED" } else { "OPEN" }),
                Col::Title => Cell::plain(title.clone()),
                Col::Context => Cell::plain(context.clone()),
                Col::Due => match due_state {
                    DueState::Overdue => Cell::colored(due.clone(), |p, s| p.red(s)),
                    DueState::Today => Cell::colored(due.clone(), |p, s| p.yellow(s)),
                    DueState::Upcoming => Cell::plain(due.clone()),
                },
                // Never shortened to `about 9 hours...`: the title gives way.
                Col::Updated => Cell::fixed(updated.clone(), |p, s| p.muted(s)),
            })
            .collect();
        table.add_row(row);
    }
    table.render()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn summary_reads_like_gh() {
        // reference/gh/issue-list.txt: "Showing 30 of 1022 open issues in cli/cli"
        assert_eq!(
            summary_line(30, Total::Exact(1022), "assigned to you in Acme", false),
            "Showing 30 of 1022 open tasks assigned to you in Acme"
        );
        assert_eq!(
            summary_line(5, Total::Exact(5), "in Roadmap", false),
            "Showing 5 of 5 open tasks in Roadmap"
        );
        assert_eq!(
            summary_line(1, Total::Exact(1), "in Roadmap", false),
            "Showing 1 of 1 open task in Roadmap"
        );
        // A My Tasks list has no counter: `100+` once a page is known to
        // be fewer than all.
        assert_eq!(
            summary_line(30, Total::AtLeast(100), "assigned to you in Acme", false),
            "Showing 30 of 100+ open tasks assigned to you in Acme"
        );
        // reference/gh/issue-list-filtered.txt:
        // "Showing 8 of 2248 issues in cli/cli that match your search"
        assert_eq!(
            summary_line(8, Total::Exact(2248), "in Roadmap", true),
            "Showing 8 of 2248 tasks in Roadmap that match your search"
        );
        assert_eq!(
            summary_line(1, Total::Exact(1), "in Acme", true),
            "Showing 1 of 1 task in Acme that matches your search"
        );
        assert_eq!(
            summary_line(30, Total::AtLeast(100), "in Acme", true),
            "Showing 30 of 100+ tasks in Acme that match your search"
        );
    }

    #[test]
    fn empty_reads_like_gh() {
        // reference/gh/issue-list-empty.txt: "no issues match your search in cli/cli"
        assert_eq!(empty_line("in Roadmap", false), "no open tasks in Roadmap");
        assert_eq!(
            empty_line("in Roadmap", true),
            "no tasks match your search in Roadmap"
        );
        assert_eq!(
            empty_line("assigned to you in Acme", false),
            "no open tasks assigned to you in Acme"
        );
    }

    #[test]
    fn scope_names_project_or_workspace() {
        let mut listing = Listing {
            items: Vec::new(),
            total: Total::Exact(0),
            project: None,
            workspace: Some(Workspace {
                gid: "1".into(),
                name: Some("Acme".into()),
            }),
        };
        assert_eq!(listing.scope(true), "assigned to you in Acme");
        assert_eq!(listing.scope(false), "in Acme");
        listing.project = Some(Project {
            gid: "2".into(),
            name: Some("Roadmap".into()),
            ..Project::default()
        });
        assert_eq!(listing.scope(true), "in Roadmap");
        assert_eq!(listing.scope(false), "in Roadmap");
        assert_eq!(
            summary_line(3, Total::Exact(12), &listing.scope(true), false),
            "Showing 3 of 12 open tasks in Roadmap"
        );
    }

    #[test]
    fn cells_stay_on_one_line() {
        assert_eq!(one_line("a\nb\tc\r\n"), "a b c");
        assert_eq!(one_line("plain"), "plain");
    }

    #[test]
    fn context_column_is_projects_or_assignee() {
        let t = Task {
            assignee: Some(Named {
                gid: "9".into(),
                name: Some("Jane Doe".into()),
                email: None,
            }),
            projects: vec![
                Named {
                    gid: "1".into(),
                    name: Some("Roadmap".into()),
                    email: None,
                },
                Named {
                    gid: "2".into(),
                    name: Some("Q4".into()),
                    email: None,
                },
            ],
            ..Task::default()
        };
        assert_eq!(context_column(&t, false), "Roadmap, Q4");
        assert_eq!(context_column(&t, true), "Jane Doe");
        assert_eq!(context_column(&Task::default(), true), "");
    }

    #[test]
    fn state_filter_keeps_what_it_says() {
        let open = Task::default();
        let done = Task {
            completed: Some(true),
            ..Task::default()
        };
        assert!(wanted(State::Open, &open) && !wanted(State::Open, &done));
        assert!(!wanted(State::Completed, &open) && wanted(State::Completed, &done));
        assert!(wanted(State::All, &open) && wanted(State::All, &done));
    }

    #[test]
    fn decode_keeps_the_raw_object_beside_the_task() {
        let raw = vec![
            serde_json::json!({ "gid": "1", "name": "One", "completed": true }),
            serde_json::json!({ "gid": "2", "name": "Two" }),
        ];
        let items = decode(raw);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].1.gid, "1");
        assert!(items[0].1.is_completed());
        assert_eq!(items[0].0["name"], "One");
        assert_eq!(items[1].1.display(), "Two");
    }

    fn listing(items: Vec<(Value, Task)>) -> Listing {
        Listing {
            total: Total::Exact(items.len()),
            items,
            project: None,
            workspace: Some(Workspace {
                gid: "1".into(),
                name: Some("Acme".into()),
            }),
        }
    }

    fn full_task(gid: &str, name: &str, due_days: Option<i64>, updated_secs: i64) -> (Value, Task) {
        let now = Utc.timestamp_opt(1_800_000_000, 0).single().expect("ts");
        let today = now.date_naive();
        let t = Task {
            gid: gid.to_string(),
            name: Some(name.to_string()),
            due_on: due_days.map(|d| today + chrono::Duration::days(d)),
            modified_at: Some(now - chrono::Duration::seconds(updated_secs)),
            created_at: Some(now),
            projects: vec![Named {
                gid: "2".into(),
                name: Some("Roadmap".into()),
                email: None,
            }],
            ..Task::default()
        };
        (Value::Null, t)
    }

    #[test]
    fn columns_give_way_from_the_middle_as_the_terminal_narrows() {
        use Col::*;
        assert_eq!(
            columns(true, 120, false),
            vec![Id, Title, Context, Due, Updated]
        );
        assert_eq!(
            columns(true, 100, false),
            vec![Id, Title, Context, Due, Updated]
        );
        assert_eq!(columns(true, 99, false), vec![Id, Title, Due, Updated]);
        assert_eq!(columns(true, 80, false), vec![Id, Title, Due, Updated]);
        assert_eq!(columns(true, 79, false), vec![Id, Title, Updated]);
        assert_eq!(columns(true, 60, false), vec![Id, Title, Updated]);
        // With a STATE column (--state all/completed) it comes before DUE
        // and CONTEXT, and is the last to go.
        assert_eq!(
            columns(true, 120, true),
            vec![Id, State, Title, Context, Due, Updated]
        );
        assert_eq!(
            columns(true, 100, true),
            vec![Id, State, Title, Due, Updated]
        );
        assert_eq!(columns(true, 80, true), vec![Id, State, Title, Updated]);
        assert_eq!(columns(true, 60, true), vec![Id, Title, Updated]);
        // piped output always carries every column
        assert_eq!(
            columns(false, 40, false),
            vec![Id, State, Title, Context, Due, Updated]
        );
    }

    #[test]
    fn narrow_terminal_still_fits_like_gh() {
        // reference/gh/issue-list-narrow.txt is captured at 60 columns.
        let now = Utc.timestamp_opt(1_800_000_000, 0).single().expect("ts");
        let long = "gh pr merge NNN --delete-branch fails to delete branch unless \"automatically delete head branches\" is toggled on";
        let l = listing(vec![
            full_task("1211000000000040", long, None, 9 * 3600),
            full_task(
                "1211000000000036",
                "Support deleting edit-history revisions",
                Some(-2),
                86_400,
            ),
        ]);
        let out = render_table(true, 60, false, &l, State::Open, now, now.date_naive());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(crate::render::text::width(line) <= 60, "{line:?}");
        }
        assert_eq!(
            lines[0],
            "ID                TITLE                    UPDATED          "
        );
        assert_eq!(
            lines[1],
            "1211000000000040  gh pr merge NNN --de...  about 9 hours ago"
        );
        assert_eq!(
            lines[2],
            "1211000000000036  Support deleting edi...  about 1 day ago"
        );
    }

    #[test]
    fn updated_is_never_shortened_the_title_gives_way() {
        let now = Utc.timestamp_opt(1_800_000_000, 0).single().expect("ts");
        let l = listing(vec![full_task(
            "1211000000000040",
            "A title long enough to need shortening on a narrow terminal",
            None,
            30,
        )]);
        let out = render_table(true, 60, false, &l, State::Open, now, now.date_naive());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[1],
            "1211000000000040  A title long en...  less than a minute ago"
        );
        assert!(crate::render::text::width(lines[1]) <= 60);
    }

    #[test]
    fn wide_terminal_has_five_columns_and_natural_dates() {
        let now = Utc.timestamp_opt(1_800_000_000, 0).single().expect("ts");
        let l = listing(vec![
            full_task("1211000000000040", "Write the Q4 planning doc", Some(0), 60),
            full_task(
                "1211000000000036",
                "Rotate the deploy keys",
                Some(-2),
                86_400,
            ),
        ]);
        let out = render_table(true, 120, false, &l, State::Open, now, now.date_naive());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "ID                TITLE                      PROJECT  DUE             UPDATED           "
        );
        assert_eq!(
            lines[1],
            "1211000000000040  Write the Q4 planning doc  Roadmap  today           about 1 minute ago"
        );
        assert_eq!(
            lines[2],
            "1211000000000036  Rotate the deploy keys     Roadmap  2 days overdue  about 1 day ago"
        );
    }

    #[test]
    fn state_all_adds_a_state_column_after_the_id() {
        let now = Utc.timestamp_opt(1_800_000_000, 0).single().expect("ts");
        let mut l = listing(vec![
            full_task("1211000000000040", "Write the Q4 planning doc", Some(0), 60),
            full_task("1211000000000036", "Rotate the deploy keys", Some(0), 60),
        ]);
        l.items[1].1.completed = Some(true);
        let out = render_table(true, 120, false, &l, State::All, now, now.date_naive());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "ID                STATE      TITLE                      PROJECT  DUE    UPDATED           "
        );
        assert_eq!(
            lines[1],
            "1211000000000040  open       Write the Q4 planning doc  Roadmap  today  about 1 minute ago"
        );
        // The same date reads the same way whatever the state.
        assert_eq!(
            lines[2],
            "1211000000000036  completed  Rotate the deploy keys     Roadmap  today  about 1 minute ago"
        );
        // The state cell is painted like the id: green open, magenta done.
        let painted = render_table(true, 120, true, &l, State::All, now, now.date_naive());
        assert!(painted.contains("\x1b[0;32m1211000000000040\x1b[0m  \x1b[0;32mopen     \x1b[0m  "));
        assert!(painted.contains("\x1b[0;35m1211000000000036\x1b[0m  \x1b[0;35mcompleted\x1b[0m  "));
        // Only when completed tasks can be in the listing.
        let open_only = render_table(true, 120, false, &l, State::Open, now, now.date_naive());
        assert!(!open_only.contains("STATE"));
        let done_only = render_table(
            true,
            120,
            false,
            &l,
            State::Completed,
            now,
            now.date_naive(),
        );
        assert!(done_only.starts_with("ID                STATE      TITLE"));
    }

    #[test]
    fn piped_output_is_tsv_with_iso_dates_and_no_header() {
        // reference/gh/issue-list-piped.txt: `14404\tOPEN\t<title>\t<labels>\t2026-09-09T03:16:57Z`
        let now = Utc.timestamp_opt(1_800_000_000, 0).single().expect("ts");
        let mut l = listing(vec![full_task(
            "1211000000000040",
            "Write the\tQ4 planning doc",
            Some(1),
            60,
        )]);
        l.items[0].1.completed = Some(true);
        let out = render_table(false, 40, false, &l, State::Open, now, now.date_naive());
        let today = now.date_naive();
        let due = (today + chrono::Duration::days(1)).to_string();
        let updated = iso(now - chrono::Duration::seconds(60));
        assert_eq!(
            out,
            format!("1211000000000040\tCOMPLETED\tWrite the Q4 planning doc\tRoadmap\t{due}\t{updated}\n")
        );
    }

    #[test]
    fn completed_tasks_show_the_date_they_were_due_not_overdue() {
        let now = Utc.timestamp_opt(1_800_000_000, 0).single().expect("ts");
        let today = now.date_naive();
        let mut l = listing(vec![
            full_task("1211000000000040", "Ship it", Some(-3), 60),
            full_task("1211000000000036", "Shipped", Some(-3), 60),
            full_task("1211000000000030", "Shipped early", Some(1), 60),
        ]);
        l.items[1].1.completed = Some(true);
        l.items[2].1.completed = Some(true);
        let out = render_table(true, 120, false, &l, State::All, now, today);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("3 days overdue"), "{:?}", lines[1]);
        let due = short_date(today - chrono::Duration::days(3), today);
        assert!(lines[2].contains(&due), "{:?}", lines[2]);
        assert!(!lines[2].contains("overdue"), "{:?}", lines[2]);
        assert!(lines[3].contains("tomorrow"), "{:?}", lines[3]);
        // piped: ISO dates whatever the state
        let out = render_table(false, 80, false, &l, State::All, now, today);
        let iso_due = (today - chrono::Duration::days(3)).to_string();
        assert_eq!(out.matches(&iso_due).count(), 2);
    }

    fn args(limit: usize, json: &[&str], jq: Option<&str>) -> Args {
        Args {
            project: None,
            assignee: None,
            state: State::Open,
            limit,
            json: JsonArgs {
                json: json.iter().map(|s| s.to_string()).collect(),
                jq: jq.map(str::to_string),
            },
            web: false,
        }
    }

    #[test]
    fn limit_must_be_at_least_one() {
        use crate::cmd::Prevalidate;
        let err = args(0, &[], None).prevalidate(true).expect_err("zero");
        assert_eq!(err.to_string(), "invalid limit: 0");
        assert!(args(1, &[], None).prevalidate(false).is_ok());
    }

    #[test]
    fn json_fields_are_checked_before_the_token_is_read() {
        use crate::cmd::Prevalidate;
        assert!(args(30, &["gid,name,due_on"], None)
            .prevalidate(true)
            .is_ok());
        assert!(args(30, &["assignee.name"], None).prevalidate(true).is_ok());
        assert!(args(30, &[""], Some(".[].gid")).prevalidate(true).is_ok());
        let err = args(30, &["gid,nope"], None)
            .prevalidate(true)
            .expect_err("unknown field");
        assert!(
            err.to_string()
                .starts_with("Unknown JSON field: \"nope\"\nAvailable fields:\n  assignee\n"),
            "{err}"
        );
        let err = args(30, &[], Some("."))
            .prevalidate(true)
            .expect_err("lone jq");
        assert_eq!(
            err.to_string(),
            "cannot use `--jq` without specifying `--json`"
        );
        // The advertised list is what the help prints and what every raw
        // object carries at its top level.
        assert_eq!(Args::JSON_FIELDS, JSON_FIELDS);
        for field in TASK_FIELDS.split(',') {
            let head = field.split('.').next().expect("field");
            assert!(
                JSON_FIELDS.contains(&head),
                "{head} is fetched but not advertised"
            );
        }
    }

    #[test]
    fn json_output_keeps_only_the_fields_asked_for() {
        let raw = Value::Array(vec![
            serde_json::json!({ "gid": "1", "name": "One", "due_on": "2026-09-12", "notes": "x" }),
            serde_json::json!({ "gid": "2", "name": "Two", "due_on": null, "notes": "" }),
        ]);
        let a = args(30, &["gid,name"], None);
        assert_eq!(
            json::render(&raw, &a.json, JSON_FIELDS).expect("render"),
            "[\n  {\n    \"gid\": \"1\",\n    \"name\": \"One\"\n  },\n  {\n    \"gid\": \"2\",\n    \"name\": \"Two\"\n  }\n]\n"
        );
        let a = args(30, &["name"], Some(".[].name"));
        assert_eq!(
            json::render(&raw, &a.json, JSON_FIELDS).expect("render"),
            "One\nTwo\n"
        );
    }

    /// The help prose is hand-wrapped short of 78 columns like gh's, and
    /// says where the output formats are documented.
    #[test]
    fn help_prose_is_hand_wrapped_like_gh() {
        use clap::Args as _;
        let cmd = Args::augment_args(clap::Command::new("list"));
        let about = cmd.get_long_about().expect("long_about").to_string();
        for line in about.lines() {
            assert!(line.chars().count() <= 80, "wider than 80: {line}");
            assert!(line.trim_end() == line, "trailing space: {line:?}");
        }
        assert!(about.contains("see `asana help formatting`."));
        let examples = cmd.get_after_help().expect("examples").to_string();
        for line in examples.lines() {
            assert!(line.starts_with("$ asana task list"), "{line}");
            assert!(line.chars().count() + 2 <= 78, "wider than 78: {line}");
        }
        assert!(examples.contains("--json gid,name,due_on"));
        assert!(examples.contains("--jq"));
    }

    #[test]
    fn state_accepts_closed_as_completed() {
        assert_eq!(
            State::from_str("closed", true).expect("alias"),
            State::Completed
        );
        assert_eq!(State::from_str("open", true).expect("open"), State::Open);
        assert!(State::from_str("bogus", true).is_err());
    }
}
