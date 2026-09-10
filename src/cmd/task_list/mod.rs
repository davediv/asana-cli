//! `asana task list`: tasks assigned to you, or the tasks of a project.
//!
//! Modelled on `gh issue list` (see `reference/gh/issue-list*.txt`): a blank
//! line, a `Showing N of M ...` summary, a blank line, then a table with a
//! dim underlined header, a green (open) or magenta (completed) identifier
//! column, and a muted relative timestamp. Piped, it is tab-separated with no
//! header and ISO-8601 dates; `--json` is the raw API array.

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde_json::Value;

use crate::api::{Named, Project, Task, Workspace, TASK_FIELDS};
use crate::cli::usage_error;
use crate::cmd::{open_in_browser, print_json, with_spinner};
use crate::ctx::Ctx;
use crate::render::table::Cell;
use crate::render::time::{due_phrase, fuzzy_ago, iso, now, short_date, today, DueState};
use crate::render::{Palette, Table};
use crate::resolve::{resolve_assignee_value, resolve_project};

/// The command's path, for usage errors.
const PATH: &[&str] = &["task", "list"];

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
        assigned to you.\n\n\
        Use `--project` to list the tasks of a project instead. A project can be\n\
        given by gid, by URL, or by name (case-insensitive; a unique prefix is\n\
        enough). Tasks are shown newest first.\n\n\
        When stdout is not a terminal the output is tab-separated with no header,\n\
        one task per line, with ISO-8601 dates.\n\n\
        For more information about output formatting, see `asana help formatting`.",
    after_help = "$ asana task list\n\
        $ asana task list --project \"Roadmap\"\n\
        $ asana task list --assignee \"jane@example.com\"\n\
        $ asana task list --assignee \"@me\" --state all\n\
        $ asana task list --state completed --limit 50\n\
        $ asana task list --project Roadmap --json"
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

    /// Output the raw API objects as JSON
    #[arg(long)]
    pub json: bool,

    /// List tasks in the web browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

/// Which columns a terminal gets. gh never drops columns, but its IDs are
/// six characters; a 16-digit gid would starve the title on a narrow
/// terminal, so the middle columns give way first and UPDATED stays last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// ID, TITLE, PROJECT|ASSIGNEE, DUE, UPDATED (and always when piped).
    Full,
    /// ID, TITLE, DUE, UPDATED.
    NoContext,
    /// ID, TITLE, UPDATED.
    Minimal,
}

impl Layout {
    fn for_terminal(is_tty: bool, width: usize) -> Self {
        if !is_tty || width >= 100 {
            Layout::Full
        } else if width >= 80 {
            Layout::NoContext
        } else {
            Layout::Minimal
        }
    }
}

/// gh's `text.Pluralize`: `1 open task`, `2 open tasks`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// gh's `ListHeader`: the line above the table. `scope` already carries its
/// preposition: `in Roadmap`, `assigned to you in Acme`.
fn summary_line(shown: usize, total: usize, scope: &str, has_filters: bool) -> String {
    if has_filters {
        let verb = if total == 1 { "matches" } else { "match" };
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

/// Newest first, like gh (created date, then gid as a tiebreak).
fn sort_newest_first(items: &mut [(Value, Task)]) {
    let gid_num = |t: &Task| t.gid.parse::<u64>().unwrap_or(0);
    items.sort_by(|a, b| {
        b.1.created_at
            .cmp(&a.1.created_at)
            .then_with(|| gid_num(&b.1).cmp(&gid_num(&a.1)))
    });
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

/// Everything the API gave us, already filtered, sorted and counted.
struct Listing {
    items: Vec<(Value, Task)>,
    total: usize,
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

/// Resolve identifiers and fetch the tasks. Separated from `run` so the
/// spinner can be stopped before anything is printed, success or error.
async fn fetch(ctx: &Ctx, args: &Args) -> Result<Listing> {
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

    let mut query: Vec<(&str, &str)> = vec![("opt_fields", TASK_FIELDS)];
    match (&project, &workspace) {
        (Some(p), _) => query.push(("project", p.gid.as_str())),
        (None, Some(ws)) => {
            if let Some(a) = assignee.as_deref() {
                query.push(("assignee", a));
            }
            query.push(("workspace", ws.gid.as_str()));
        }
        (None, None) => {}
    }
    if args.state == State::Open {
        // Incomplete tasks plus those completed since now, i.e. none.
        query.push(("completed_since", "now"));
    }

    let raw: Vec<Value> = ctx.api.get_all("/tasks", &query, None).await?;
    let mut items: Vec<(Value, Task)> = raw
        .into_iter()
        .filter_map(|v| {
            serde_json::from_value::<Task>(v.clone())
                .ok()
                .map(|t| (v, t))
        })
        .collect();

    // Filters the API cannot combine with `project=`: assignee, and the
    // completed-only state.
    if project.is_some() {
        if let Some(a) = &assignee {
            let wanted = if a == "me" {
                ctx.me().await?.gid
            } else {
                a.clone()
            };
            items.retain(|(_, t)| t.assignee.as_ref().is_some_and(|x| x.gid == wanted));
        }
    }
    match args.state {
        State::Open => items.retain(|(_, t)| !t.is_completed()),
        State::Completed => items.retain(|(_, t)| t.is_completed()),
        State::All => {}
    }
    sort_newest_first(&mut items);
    let total = items.len();
    items.truncate(args.limit);

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
    let my_tasks: Result<Named, _> = ctx
        .api
        .get(
            "/users/me/user_task_list",
            &[("workspace", ws.gid.as_str()), ("opt_fields", "name")],
        )
        .await;
    Ok(match my_tasks {
        Ok(list) if !list.gid.is_empty() => format!("https://app.asana.com/0/{}/list", list.gid),
        _ => format!("https://app.asana.com/0/home/{}", ws.gid),
    })
}

/// gh's own check, before the token is read: `invalid limit: 0`, a flag
/// error, so the usage block follows it. (`cli::Commands::prevalidate`
/// currently bails on the same case first, without the block.)
impl crate::cmd::Prevalidate for Args {
    fn prevalidate(&self, _interactive: bool) -> Result<()> {
        if self.limit < 1 {
            return Err(usage_error(PATH, format!("invalid limit: {}", self.limit)));
        }
        Ok(())
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if args.web {
        let url = with_spinner(ctx, "", web_url(ctx, &args)).await?;
        return open_in_browser(&url);
    }

    let listing = with_spinner(ctx, "", fetch(ctx, &args)).await?;

    if args.json {
        let values: Vec<Value> = listing.items.into_iter().map(|(v, _)| v).collect();
        return print_json(&values);
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
        render_table(ctx.is_tty, ctx.width, ctx.color, &listing, now(), today())
    );
    Ok(())
}

/// Build the table for the listing. `now`/`today` are passed in so tests
/// can pin the relative phrases.
fn render_table(
    is_tty: bool,
    width: usize,
    color: bool,
    listing: &Listing,
    now: DateTime<Utc>,
    today: chrono::NaiveDate,
) -> String {
    let listing_project = listing.project.is_some();
    let layout = Layout::for_terminal(is_tty, width);
    let mut headers = vec!["ID", "TITLE"];
    if layout == Layout::Full {
        headers.push(if listing_project {
            "ASSIGNEE"
        } else {
            "PROJECT"
        });
    }
    if layout != Layout::Minimal {
        headers.push("DUE");
    }
    headers.push("UPDATED");

    let mut table = Table::new(&headers, is_tty, width, Palette::new(color));
    for (_, task) in &listing.items {
        let title = one_line(task.display());
        let context = one_line(&context_column(task, listing_project));
        // A completed task is not overdue: it shows the date it was due,
        // plain; an open one reads `today`, `tomorrow`, `3 days overdue`.
        let (due, due_state) = match due_date(task) {
            Some(d) if !is_tty => (d.to_string(), DueState::Upcoming),
            Some(d) if task.is_completed() => (short_date(d, today), DueState::Upcoming),
            Some(d) => due_phrase(d, today),
            None => (String::new(), DueState::Upcoming),
        };
        let updated = match task.modified_at {
            Some(t) if is_tty => fuzzy_ago(now, t),
            Some(t) => iso(t),
            None => String::new(),
        };
        let due_cell = match due_state {
            DueState::Overdue => Cell::colored(due, |p, s| p.red(s)),
            DueState::Today => Cell::colored(due, |p, s| p.yellow(s)),
            DueState::Upcoming => Cell::plain(due),
        };

        let mut row = Vec::with_capacity(6);
        if is_tty {
            // Green for open, magenta for completed: gh's open/closed colours.
            row.push(if task.is_completed() {
                Cell::fixed(task.gid.clone(), |p, s| p.magenta(s))
            } else {
                Cell::fixed(task.gid.clone(), |p, s| p.green(s))
            });
            row.push(Cell::plain(title));
        } else {
            row.push(Cell::plain(task.gid.clone()));
            row.push(Cell::plain(if task.is_completed() {
                "COMPLETED"
            } else {
                "OPEN"
            }));
            row.push(Cell::plain(title));
        }
        if layout == Layout::Full {
            row.push(Cell::plain(context));
        }
        if layout != Layout::Minimal {
            row.push(due_cell);
        }
        row.push(Cell::colored(updated, |p, s| p.muted(s)));
        table.add_row(row);
    }
    table.render()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn task(gid: &str, created: i64) -> (Value, Task) {
        let t = Task {
            gid: gid.to_string(),
            created_at: Some(Utc.timestamp_opt(created, 0).single().expect("ts")),
            ..Task::default()
        };
        (Value::Null, t)
    }

    #[test]
    fn summary_reads_like_gh() {
        // reference/gh/issue-list.txt: "Showing 30 of 1022 open issues in cli/cli"
        assert_eq!(
            summary_line(30, 1022, "assigned to you in Acme", false),
            "Showing 30 of 1022 open tasks assigned to you in Acme"
        );
        assert_eq!(
            summary_line(5, 5, "in Roadmap", false),
            "Showing 5 of 5 open tasks in Roadmap"
        );
        assert_eq!(
            summary_line(1, 1, "in Roadmap", false),
            "Showing 1 of 1 open task in Roadmap"
        );
        // reference/gh/issue-list-filtered.txt:
        // "Showing 8 of 2248 issues in cli/cli that match your search"
        assert_eq!(
            summary_line(8, 2248, "in Roadmap", true),
            "Showing 8 of 2248 tasks in Roadmap that match your search"
        );
        assert_eq!(
            summary_line(1, 1, "in Acme", true),
            "Showing 1 of 1 task in Acme that matches your search"
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
            total: 0,
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
            summary_line(3, 12, &listing.scope(true), false),
            "Showing 3 of 12 open tasks in Roadmap"
        );
    }

    #[test]
    fn sorts_newest_first_with_gid_tiebreak() {
        let mut items = vec![
            task("1000000000000001", 100),
            task("1000000000000003", 300),
            task("1000000000000002", 300),
            task("999", 50),
        ];
        sort_newest_first(&mut items);
        let gids: Vec<&str> = items.iter().map(|(_, t)| t.gid.as_str()).collect();
        assert_eq!(
            gids,
            vec![
                "1000000000000003",
                "1000000000000002",
                "1000000000000001",
                "999"
            ]
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

    fn listing(items: Vec<(Value, Task)>) -> Listing {
        Listing {
            total: items.len(),
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
    fn layout_drops_middle_columns_as_the_terminal_narrows() {
        assert_eq!(Layout::for_terminal(true, 120), Layout::Full);
        assert_eq!(Layout::for_terminal(true, 100), Layout::Full);
        assert_eq!(Layout::for_terminal(true, 99), Layout::NoContext);
        assert_eq!(Layout::for_terminal(true, 80), Layout::NoContext);
        assert_eq!(Layout::for_terminal(true, 60), Layout::Minimal);
        // piped output always carries every column
        assert_eq!(Layout::for_terminal(false, 40), Layout::Full);
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
        let out = render_table(true, 60, false, &l, now, now.date_naive());
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
        let out = render_table(true, 120, false, &l, now, now.date_naive());
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
        let out = render_table(false, 40, false, &l, now, now.date_naive());
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
        ]);
        l.items[1].1.completed = Some(true);
        let out = render_table(true, 120, false, &l, now, today);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("3 days overdue"), "{:?}", lines[1]);
        let due = short_date(today - chrono::Duration::days(3), today);
        assert!(lines[2].contains(&due), "{:?}", lines[2]);
        assert!(!lines[2].contains("overdue"), "{:?}", lines[2]);
        // piped: ISO dates whatever the state
        let out = render_table(false, 80, false, &l, now, today);
        let iso_due = (today - chrono::Duration::days(3)).to_string();
        assert_eq!(out.matches(&iso_due).count(), 2);
    }

    #[test]
    fn limit_must_be_at_least_one() {
        use crate::cmd::Prevalidate;
        let args = |limit: usize| Args {
            project: None,
            assignee: None,
            state: State::Open,
            limit,
            json: false,
            web: false,
        };
        let err = args(0).prevalidate(true).expect_err("zero");
        assert_eq!(err.to_string(), "invalid limit: 0");
        assert!(args(1).prevalidate(false).is_ok());
    }

    /// The help prose is hand-wrapped short of 78 columns like gh's, and
    /// says where the output formats are documented.
    #[test]
    fn help_prose_is_hand_wrapped_like_gh() {
        use clap::Args as _;
        let cmd = Args::augment_args(clap::Command::new("list"));
        let about = cmd.get_long_about().expect("long_about").to_string();
        for line in about.lines() {
            assert!(line.chars().count() <= 78, "wider than 78: {line}");
            assert!(line.trim_end() == line, "trailing space: {line:?}");
        }
        assert!(about.contains("see `asana help formatting`."));
        let examples = cmd.get_after_help().expect("examples").to_string();
        for line in examples.lines() {
            assert!(line.starts_with("$ asana task list"), "{line}");
            assert!(line.chars().count() + 2 <= 78, "wider than 78: {line}");
        }
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
