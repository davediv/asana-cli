//! `asana project list`: the projects in the workspace, in the form of
//! `gh repo list` (see `reference/gh/repo-list*.txt`).
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
//! NAME is bold, UPDATED is muted, INFO is muted (yellow when the project is
//! private, as gh paints private repositories), rows are newest-modified
//! first, and the widest column gives way to `...` first. A column that no
//! project fills is left out: a workspace whose projects carry no notes gets
//! `NAME  INFO  UPDATED`, not thirteen columns of blank DESCRIPTION. Piped:
//! `gid  name  description  info  modified` separated by tabs, no header,
//! ISO-8601 time, every field there whether or not it is empty.
//! The table itself is the shared `render::Table`, which pads, truncates and
//! paints in the bytes gh emits; the tests at the bottom hold it to the
//! reference layout.
//!
//! `--json [fields]` and `-q, --jq <expression>` are gh's (the shared
//! `render::json`): the raw API objects as an array, only the named
//! top-level keys when a field list is given, filtered through a jq program
//! under `--jq`. [`JSON_FIELDS`] is what a field may be, and what `--help`
//! lists under JSON FIELDS.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::Project;
use crate::cli::usage_error;
use crate::cmd::{open_in_browser, with_spinner};
use crate::ctx::Ctx;
use crate::render::json::{self, JsonArgs};
use crate::render::table::Cell;
use crate::render::time::{fuzzy_ago, iso, now};
use crate::render::{Palette, Table};

/// The command's path, for usage errors.
const PATH: &[&str] = &["project", "list"];

/// What `project list` asks Asana for. `api::PROJECT_FIELDS` with one
/// change: `members.name` where the shared list asks for bare `members`,
/// which comes back as `{"gid", "resource_type"}` per person and names
/// nobody. Same size on the wire, so `--json members` answers "who is on
/// this project?" instead of listing gids. (A wish for the shared list.)
const PROJECT_FIELDS: &str = "name,archived,color,due_on,modified_at,owner.name,\
    current_status.title,current_status.color,team.name,permalink_url,notes,\
    members.name,created_at,public,privacy_setting,completed,start_on";

/// Every top-level key of a project as `--json` prints it: `gid` plus the
/// fields `PROJECT_FIELDS` asks Asana for, with the dotted ones
/// (`owner.name`, `current_status.title`, `team.name`) under their parent.
/// `--json` takes these, and `--help` lists them under JSON FIELDS.
const JSON_FIELDS: &[&str] = &[
    "gid",
    "name",
    "archived",
    "color",
    "created_at",
    "due_on",
    "modified_at",
    "owner",
    "current_status",
    "team",
    "permalink_url",
    "notes",
    "privacy_setting",
    "public",
    "completed",
    "start_on",
    "members",
];

/// A cell painter, as `render::table::Cell::colored` takes it.
type ColorFn = fn(&Palette, &str) -> String;

#[derive(clap::Args, Debug)]
#[command(
    about = "List projects in the workspace",
    long_about = "List projects in the workspace.\n\n\
        Note that the list will only include projects that are not archived; use the\n\
        `--archived` flag to list the archived ones instead. Projects are shown most\n\
        recently modified first, and the INFO column carries a project's visibility\n\
        (public or private), its latest status update (on track, at risk, off track,\n\
        on hold, complete) and whether it is archived. A column that no project\n\
        fills, such as DESCRIPTION where none of them carries notes, is left out\n\
        of the table; piped output keeps every field, blank or not, so `cut -f2`\n\
        is always the name.\n\n\
        A name from the NAME column is accepted by every command that takes a\n\
        project, for example `asana task list --project \"Roadmap\"`; a\n\
        case-insensitive unique prefix is enough.\n\n\
        `--json` without a field list prints every field of every project; name\n\
        the ones you want, or filter what it prints with `--jq`. For more\n\
        information about output formatting, see `asana help formatting`.",
    after_help = "$ asana project list\n\
        $ asana project list --archived\n\
        $ asana project list --limit 5\n\
        $ asana project list --json gid,name,permalink_url\n\
        $ asana project list --json name --jq '.[].name'\n\
        $ asana project list | cut -f1,2"
)]
pub struct Args {
    /// Show only archived projects
    #[arg(long)]
    pub archived: bool,

    /// Maximum number of projects to list
    #[arg(short = 'L', long, value_name = "int", default_value_t = 30)]
    pub limit: usize,

    /// `--json [fields]` and `-q, --jq <expression>`, gh's output flags.
    #[command(flatten)]
    pub json: JsonArgs,

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
/// (newlines and tabs included) collapsed to one space, so a cell stays on
/// one line and, when piped, holds no tab.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The NAME column: the project's name on one line, or its gid when Asana
/// sent none.
fn name(project: &Project) -> String {
    one_line(project.display())
}

/// The DESCRIPTION column: the project's notes squeezed onto one line.
fn description(project: &Project) -> String {
    one_line(project.notes.as_deref().unwrap_or_default())
}

/// The INFO column, in the style of gh's `public, fork, archived`:
/// visibility, then the latest status update's colour as words, then
/// `complete` and `archived` flags.
fn info(project: &Project) -> String {
    let mut tags: Vec<&str> = Vec::new();
    match project.is_public() {
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
fn info_color(project: &Project) -> ColorFn {
    if project.is_public() == Some(false) {
        Palette::yellow
    } else {
        Palette::muted
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

/// When the project was last touched: `modified_at`, else `created_at`, as
/// gh falls back from `pushedAt` to `createdAt`.
fn updated_at(project: &Project) -> Option<DateTime<Utc>> {
    project.modified_at.or(project.created_at)
}

/// Most recently modified first; ties and unknown times keep API order.
fn sort_newest_first(items: &mut [Item]) {
    items.sort_by(|a, b| updated_at(&b.project).cmp(&updated_at(&a.project)));
}

/// The columns of the terminal table, in `gh repo list`'s order.
const HEADERS: [&str; 4] = ["NAME", "DESCRIPTION", "INFO", "UPDATED"];

/// The table gh would print of these rows, minus any column no row fills.
///
/// gh always prints all four, because a repository nearly always carries a
/// description. Asana projects often carry none, and a column of nothing
/// but its own header standing between NAME and INFO is thirteen columns
/// of the terminal spent saying nothing, so a column every row leaves empty
/// is left out. NAME always stays, and a table with no rows keeps them all.
fn assemble(headers: &[&str], rows: Vec<Vec<Cell>>, width: usize, palette: Palette) -> Table {
    let keep: Vec<bool> = (0..headers.len())
        .map(|i| {
            i == 0
                || rows.is_empty()
                || rows
                    .iter()
                    .any(|r| r.get(i).is_some_and(|c| !c.text.is_empty()))
        })
        .collect();
    let kept: Vec<&str> = headers
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(h, _)| *h)
        .collect();
    let mut table = Table::new(&kept, true, width, palette);
    for row in rows {
        table.add_row(
            row.into_iter()
                .zip(&keep)
                .filter(|(_, k)| **k)
                .map(|(c, _)| c)
                .collect(),
        );
    }
    table
}

/// The terminal table: gh's `NAME  DESCRIPTION  INFO  UPDATED`, laid out
/// for `width` columns and painted with `palette`; UPDATED is relative to
/// `now`.
fn terminal_table(items: &[Item], width: usize, palette: Palette, now: DateTime<Utc>) -> Table {
    let rows: Vec<Vec<Cell>> = items
        .iter()
        .map(|item| {
            let p = &item.project;
            let updated = updated_at(p).map(|t| fuzzy_ago(now, t)).unwrap_or_default();
            vec![
                Cell::colored(name(p), Palette::bold),
                Cell::plain(description(p)),
                Cell::colored(info(p), info_color(p)),
                Cell::colored(updated, Palette::muted),
            ]
        })
        .collect();
    assemble(&HEADERS, rows, width, palette)
}

/// The piped table: `gid  name  description  info  modified` per line,
/// tab-separated, no header, ISO-8601 time, nothing truncated.
fn piped_table(items: &[Item]) -> Table {
    let mut table = Table::tsv();
    for item in items {
        let p = &item.project;
        table.add_row(vec![
            Cell::plain(p.gid.clone()),
            Cell::plain(name(p)),
            Cell::plain(description(p)),
            Cell::plain(info(p)),
            Cell::plain(updated_at(p).map(iso).unwrap_or_default()),
        ]);
    }
    table
}

/// gh's own checks, before the token is read, in the order gh runs them:
/// the `--json` flags first (an unknown field, `--jq` without `--json`,
/// `--web` with `--json`; plain errors, no usage block), then `invalid
/// limit: 0`, a flag error that the usage block follows. `JSON_FIELDS` is
/// what `--help` prints under JSON FIELDS.
impl crate::cmd::Prevalidate for Args {
    const JSON_FIELDS: &'static [&'static str] = JSON_FIELDS;

    fn prevalidate(&self, _interactive: bool) -> Result<()> {
        self.json.check(Self::JSON_FIELDS)?;
        if self.web && self.json.is_json() {
            bail!("cannot use `--web` with `--json`");
        }
        if self.limit < 1 {
            return Err(usage_error(PATH, format!("invalid limit: {}", self.limit)));
        }
        Ok(())
    }
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
    let raw: Vec<Value> = with_spinner(ctx, "", ctx.api.get_all("/projects", &query, None)).await?;
    let mut items: Vec<Item> = raw.into_iter().map(Item::from_value).collect();
    sort_newest_first(&mut items);
    let total = items.len();
    items.truncate(args.limit.max(1));

    if args.json.is_json() {
        let values: Vec<Value> = items.into_iter().map(|i| i.raw).collect();
        return json::print(&Value::Array(values), &args.json, JSON_FIELDS);
    }

    if !ctx.is_tty {
        print!("{}", piped_table(&items).render());
        return Ok(());
    }

    println!();
    println!(
        "{}",
        header_line(ws.display(), items.len(), total, args.archived)
    );
    println!();
    if items.is_empty() {
        return Ok(());
    }
    let table = terminal_table(&items, ctx.width, Palette::new(ctx.color), now());
    print!("{}", table.render());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ProjectStatus;
    use crate::render::text::width;
    use serde_json::json;

    fn project(v: Value) -> Project {
        serde_json::from_value(v).expect("project json")
    }

    fn item(v: Value) -> Item {
        Item::from_value(v)
    }

    /// A fixed clock for the UPDATED column.
    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("timestamp")
    }

    #[test]
    fn description_collapses_whitespace_like_gh() {
        let p = project(json!({"gid": "1", "notes": "  Everything about\n\nthe   relaunch.\t"}));
        assert_eq!(description(&p), "Everything about the relaunch.");
        assert_eq!(description(&project(json!({"gid": "1"}))), "");
        assert_eq!(description(&project(json!({"gid": "1", "notes": ""}))), "");
    }

    #[test]
    fn name_stays_on_one_line_and_falls_back_to_the_gid() {
        assert_eq!(
            name(&project(json!({"gid": "1", "name": "Road\tmap\n2"}))),
            "Road map 2"
        );
        assert_eq!(name(&project(json!({"gid": "1"}))), "1");
    }

    #[test]
    fn info_reads_like_gh_repo_flags() {
        let p = project(json!({
            "gid": "1", "public": true,
            "current_status": {"title": "All good", "color": "green"}
        }));
        assert_eq!(info(&p), "public, on track");
        assert_eq!(info_color(&p) as usize, Palette::muted as usize);

        let p = project(json!({
            "gid": "1", "public": false, "archived": true,
            "current_status": {"color": "red"}
        }));
        assert_eq!(info(&p), "private, off track, archived");
        assert_eq!(info_color(&p) as usize, Palette::yellow as usize);

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
        assert_eq!(
            info_color(&project(json!({"gid": "1"}))) as usize,
            Palette::muted as usize
        );
    }

    /// Asana is retiring `public` for `privacy_setting`; the latter wins
    /// when both are sent, and the former still works alone.
    #[test]
    fn visibility_prefers_privacy_setting() {
        let p = project(json!({"gid": "1", "public": true, "privacy_setting": "private"}));
        assert_eq!(info(&p), "private");
        assert_eq!(info_color(&p) as usize, Palette::yellow as usize);
        let p =
            project(json!({"gid": "1", "public": false, "privacy_setting": "public_to_workspace"}));
        assert_eq!(info(&p), "public");
        assert_eq!(info_color(&p) as usize, Palette::muted as usize);
        let p = project(json!({"gid": "1", "privacy_setting": "private_to_team"}));
        assert_eq!(info(&p), "private");
        let p = project(json!({"gid": "1", "public": false}));
        assert_eq!(info(&p), "private");
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

    /// A project Asana never reported a `modified_at` for sorts and shows by
    /// its `created_at`, as gh falls back from `pushedAt` to `createdAt`.
    #[test]
    fn created_at_stands_in_for_a_missing_modified_at() {
        let mut items = vec![
            item(
                json!({"gid": "old", "modified_at": "2026-09-01T00:00:00.000Z",
                "privacy_setting": "private"}),
            ),
            item(json!({"gid": "new", "created_at": "2026-09-09T00:00:00.000Z"})),
            item(json!({"gid": "none"})),
        ];
        sort_newest_first(&mut items);
        let order: Vec<&str> = items.iter().map(|i| i.project.gid.as_str()).collect();
        assert_eq!(order, vec!["new", "old", "none"]);
        assert_eq!(
            piped_table(&items).render(),
            "new\tnew\t\t\t2026-09-09T00:00:00Z\nold\told\t\tprivate\t2026-09-01T00:00:00Z\nnone\tnone\t\t\t\n"
        );
        let out =
            terminal_table(&items, 120, Palette::new(true), at("2026-09-10T00:00:00Z")).render();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[1].ends_with("\x1b[38;5;242mabout 1 day ago\x1b[0m"),
            "{:?}",
            lines[1]
        );
        // DESCRIPTION is gone (no project has one), INFO stays for the one
        // project that does; an empty INFO is still padded to the column and
        // painted, as gh paints after padding, and an unknown time in the
        // last column is nothing at all
        assert_eq!(
            lines[3],
            "\x1b[0;1;39mnone\x1b[0m  \x1b[38;5;242m       \x1b[0m  "
        );
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

    /// The rows of reference/gh/repo-list.txt as a table of ours: the same
    /// four columns, pushed through the shared table at the same 120
    /// columns, must come out as the very bytes gh printed.
    fn repo_list_table(color: bool) -> Table {
        let rows: [(&str, &str, &str, &str); 10] = [
            ("cli/go-gh", "A Go module for interacting with gh and the GitHub API from Go.", "public", "about 39 minutes ago"),
            ("cli/cli", "GitHub’s official command line tool", "public", "about 1 hour ago"),
            ("cli/browser", "Go helpers to open URLs, files, or readers in a web browser", "public, fork", "about 8 hours ago"),
            ("cli/oauth", "A library for performing OAuth Device flow and Web application flow in Go client apps.", "public", "about 2 days ago"),
            ("cli/go-internal", "Selected Go-internal packages factored out from the standard library", "public, fork", "about 7 days ago"),
            ("cli/gh-webhook", "", "public", "about 15 days ago"),
            ("cli/gh-extension-precompile", "Action for publishing binary GitHub CLI extensions", "public", "about 29 days ago"),
            ("cli/safeexec", "A safer version of exec.LookPath on Windows", "public", "about 2 years ago"),
            ("cli/shurcooL-graphql", "Package graphql provides a GraphQL client implementation.", "public, fork", "about 2 years ago"),
            ("cli/scoop-gh", "[DEPRECATED] scoop packaging for the github cli", "public, archived", "about 3 years ago"),
        ];
        // through `assemble`, the same path a real listing takes: with
        // descriptions in hand every column stays and the bytes are gh's.
        let cells = rows
            .into_iter()
            .map(|(name, desc, info, updated)| {
                vec![
                    Cell::colored(name, Palette::bold),
                    Cell::plain(desc),
                    Cell::colored(info, Palette::muted),
                    Cell::colored(updated, Palette::muted),
                ]
            })
            .collect();
        assemble(&HEADERS, cells, 120, Palette::new(color))
    }

    #[test]
    fn matches_repo_list_reference_plain() {
        let expected = "\
NAME                         DESCRIPTION                                          INFO              UPDATED             
cli/go-gh                    A Go module for interacting with gh and the GitH...  public            about 39 minutes ago
cli/cli                      GitHub’s official command line tool                  public            about 1 hour ago
cli/browser                  Go helpers to open URLs, files, or readers in a ...  public, fork      about 8 hours ago
cli/oauth                    A library for performing OAuth Device flow and W...  public            about 2 days ago
cli/go-internal              Selected Go-internal packages factored out from ...  public, fork      about 7 days ago
cli/gh-webhook                                                                    public            about 15 days ago
cli/gh-extension-precompile  Action for publishing binary GitHub CLI extensions   public            about 29 days ago
cli/safeexec                 A safer version of exec.LookPath on Windows          public            about 2 years ago
cli/shurcooL-graphql         Package graphql provides a GraphQL client implem...  public, fork      about 2 years ago
cli/scoop-gh                 [DEPRECATED] scoop packaging for the github cli      public, archived  about 3 years ago
";
        assert_eq!(repo_list_table(false).render(), expected);
    }

    /// reference/gh/repo-list.ansi.txt: `\e[0;2;4;37m` header cells,
    /// `\e[0;1;39m` names, `\e[38;5;242m` info and time, each closed by
    /// `\e[0m`, colour applied after padding, and the last column of a data
    /// row left unpadded.
    #[test]
    fn matches_repo_list_reference_ansi() {
        let out = repo_list_table(true).render();
        let mut lines = out.lines();
        assert_eq!(
            lines.next().unwrap(),
            "\x1b[0;2;4;37mNAME                       \x1b[0m  \x1b[0;2;4;37mDESCRIPTION                                        \x1b[0m  \x1b[0;2;4;37mINFO            \x1b[0m  \x1b[0;2;4;37mUPDATED             \x1b[0m"
        );
        assert_eq!(
            lines.next().unwrap(),
            "\x1b[0;1;39mcli/go-gh                  \x1b[0m  A Go module for interacting with gh and the GitH...  \x1b[38;5;242mpublic          \x1b[0m  \x1b[38;5;242mabout 39 minutes ago\x1b[0m"
        );
        assert_eq!(
            lines.next().unwrap(),
            "\x1b[0;1;39mcli/cli                    \x1b[0m  GitHub’s official command line tool                  \x1b[38;5;242mpublic          \x1b[0m  \x1b[38;5;242mabout 1 hour ago\x1b[0m"
        );
        // an empty description is plain spaces, as in gh's cli/gh-webhook row
        assert_eq!(
            lines.nth(3).unwrap(),
            "\x1b[0;1;39mcli/gh-webhook             \x1b[0m                                                       \x1b[38;5;242mpublic          \x1b[0m  \x1b[38;5;242mabout 15 days ago\x1b[0m"
        );
    }

    /// The live workspace: names with emoji (a VS16 gear, a two-cell
    /// monster, a ZWJ judge) and no descriptions at all. Every row must end
    /// its NAME cell on the same terminal column, counting emoji as the two
    /// cells a terminal draws them in, and DESCRIPTION, which no project
    /// fills, must be gone: NAME sits next to INFO.
    #[test]
    fn emoji_names_align_and_the_empty_description_column_goes() {
        let mut items = Vec::new();
        for (gid, name, at) in [
            ("1", "PR", "2026-09-10T08:02:28Z"),
            ("2", "\u{2699}\u{FE0F} UTL", "2026-09-09T15:40:13Z"),
            ("3", "\u{1F47E} FUN", "2026-09-09T11:40:13Z"),
            (
                "4",
                "\u{1F468}\u{200D}\u{2696}\u{FE0F} LEGAL",
                "2026-09-09T10:40:13Z",
            ),
            ("5", "TEMPLATE", "2026-06-09T10:40:13Z"),
        ] {
            items.push(item(json!({
                "gid": gid, "name": name, "notes": "", "privacy_setting": "private",
                "modified_at": at
            })));
        }
        let now = at("2026-09-10T09:00:00Z");
        let out = terminal_table(&items, 120, Palette::new(false), now).render();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 6);
        // NAME is 8 wide (TEMPLATE and the ZWJ judge both), INFO 7,
        // UPDATED 20 (`about 57 minutes ago`).
        assert_eq!(
            lines,
            vec![
                "NAME      INFO     UPDATED             ",
                "PR        private  about 57 minutes ago",
                "\u{2699}\u{FE0F} UTL    private  about 17 hours ago",
                "\u{1F47E} FUN    private  about 21 hours ago",
                "\u{1F468}\u{200D}\u{2696}\u{FE0F} LEGAL  private  about 22 hours ago",
                "TEMPLATE  private  about 3 months ago",
            ]
        );
        for line in &lines[1..] {
            let info_at = line.find("private").unwrap();
            assert_eq!(width(&line[..info_at]), 8 + 2, "{line:?}");
        }
        assert_eq!(width("\u{2699}\u{FE0F} UTL"), 6);
        assert_eq!(width("\u{1F47E} FUN"), 6);
        assert_eq!(width("\u{1F468}\u{200D}\u{2696}\u{FE0F} LEGAL"), 8);
    }

    /// One project with notes is enough to bring DESCRIPTION back, in gh's
    /// place for it, and the projects without notes leave the cell blank as
    /// gh's `cli/gh-webhook` row does.
    #[test]
    fn one_description_brings_the_column_back() {
        let items = vec![
            item(
                json!({"gid": "1", "name": "Roadmap", "notes": "What we ship this quarter.",
                "privacy_setting": "private", "modified_at": "2026-09-10T08:02:28Z"}),
            ),
            item(json!({"gid": "2", "name": "PR", "notes": "",
                "privacy_setting": "private", "modified_at": "2026-09-10T08:02:28Z"})),
        ];
        let out =
            terminal_table(&items, 120, Palette::new(false), at("2026-09-10T09:00:00Z")).render();
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            vec![
                "NAME     DESCRIPTION                 INFO     UPDATED             ",
                "Roadmap  What we ship this quarter.  private  about 57 minutes ago",
                "PR                                   private  about 57 minutes ago",
            ]
        );
    }

    /// Only a column every row leaves empty goes, and never NAME: a listing
    /// of projects that Asana told us nothing about but their gids is one
    /// column of gids, not four columns of nothing.
    #[test]
    fn only_columns_no_row_fills_are_dropped() {
        let items = vec![item(json!({"gid": "1"})), item(json!({"gid": "2"}))];
        let out =
            terminal_table(&items, 120, Palette::new(false), at("2026-09-10T09:00:00Z")).render();
        assert_eq!(out.lines().collect::<Vec<_>>(), vec!["NAME", "1", "2"]);
        // and nothing is dropped from a table with no rows at all
        assert_eq!(
            assemble(&HEADERS, Vec::new(), 120, Palette::new(false))
                .render()
                .trim_end(),
            "NAME  DESCRIPTION  INFO  UPDATED"
        );
    }

    /// A private project paints INFO yellow, as gh paints private
    /// repositories; a public one keeps gh's muted grey.
    #[test]
    fn private_projects_paint_info_yellow() {
        let items = vec![
            item(
                json!({"gid": "1", "name": "Secret", "privacy_setting": "private",
                "modified_at": "2026-09-10T08:02:28Z"}),
            ),
            item(
                json!({"gid": "2", "name": "Open", "privacy_setting": "public_to_workspace",
                "modified_at": "2026-09-10T08:02:28Z"}),
            ),
        ];
        let out =
            terminal_table(&items, 120, Palette::new(true), at("2026-09-10T09:00:00Z")).render();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[1].contains("\x1b[0;33mprivate\x1b[0m"),
            "{:?}",
            lines[1]
        );
        assert!(
            lines[2].contains("\x1b[38;5;242mpublic \x1b[0m"),
            "{:?}",
            lines[2]
        );
        assert!(
            lines[1].starts_with("\x1b[0;1;39mSecret\x1b[0m  "),
            "{:?}",
            lines[1]
        );
    }

    #[test]
    fn long_name_gives_way_not_the_timestamp() {
        let items = vec![item(json!({
            "gid": "1",
            "name": "Q4 marketing launch plan and campaign calendar for all regions and every channel we own",
            "notes": "Notes about the launch.",
            "privacy_setting": "public_to_workspace",
            "current_status": {"color": "yellow"},
            "modified_at": "2026-09-10T08:02:28Z"
        }))];
        // 120 columns: 114 to share, the three short columns keep their
        // natural width (23 + 15 + 20) and NAME gets the remaining 56.
        let now = at("2026-09-10T09:00:00Z");
        let out = terminal_table(&items, 120, Palette::new(false), now).render();
        let row = out.lines().nth(1).unwrap();
        assert_eq!(
            row,
            "Q4 marketing launch plan and campaign calendar for al...  Notes about the launch.  public, at risk  about 57 minutes ago"
        );
        for line in out.lines() {
            assert!(width(line) <= 120, "{line:?}");
        }
        // reference/gh/issue-list-narrow.txt: at 60 columns even UPDATED
        // becomes `about 9 hours...`; the row still fits.
        let out = terminal_table(&items, 60, Palette::new(false), now).render();
        let row = out.lines().nth(1).unwrap();
        assert_eq!(
            row,
            "Q4 marketing...  Notes abou...  public, at...  about 57 m..."
        );
        for line in out.lines() {
            assert!(width(line) <= 60, "{line:?}");
        }
    }

    /// Piped: gid, name, description, info, ISO-8601 time, tab-separated,
    /// no header, no colour, nothing truncated (reference/gh/issue-list-piped.txt).
    #[test]
    fn piped_rows_are_tab_separated_with_iso_times() {
        let items = vec![
            item(json!({
                "gid": "1213478466398849", "name": "PR", "notes": "About\tPR\nreleases",
                "privacy_setting": "private", "modified_at": "2026-09-10T08:02:28.980Z"
            })),
            item(json!({"gid": "2", "name": "Bare"})),
        ];
        assert_eq!(
            piped_table(&items).render(),
            "1213478466398849\tPR\tAbout PR releases\tprivate\t2026-09-10T08:02:28Z\n2\tBare\t\t\t\n"
        );
    }

    /// `Args` parsed the way `cli.rs` parses it, for the flag tests.
    #[derive(clap::Parser, Debug)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(argv: &[&str]) -> Args {
        use clap::Parser as _;
        Cli::try_parse_from(std::iter::once("asana").chain(argv.iter().copied()))
            .expect("parse")
            .args
    }

    /// gh's `--json` takes an optional comma-separated field list and
    /// `-q, --jq` an expression; a bare `--json` means every field, and a
    /// flag after it is not swallowed as its value.
    #[test]
    fn json_flag_takes_an_optional_field_list() {
        let none = parse(&[]);
        assert!(!none.json.is_json());
        assert_eq!(none.limit, 30);

        let bare = parse(&["--json"]);
        assert!(bare.json.is_json());
        assert!(bare.json.fields().is_empty());

        let some = parse(&["--limit", "2", "--json", "gid,name"]);
        assert_eq!(some.json.fields(), vec!["gid", "name"]);
        assert_eq!(some.limit, 2);

        let after = parse(&["--json", "--limit", "2"]);
        assert!(after.json.is_json());
        assert!(after.json.fields().is_empty());
        assert_eq!(after.limit, 2);

        let jq = parse(&["--json", "name", "-q", ".[].name"]);
        assert_eq!(jq.json.jq.as_deref(), Some(".[].name"));
        let jq = parse(&["--jq", ".[]", "--json", "name", "--archived"]);
        assert!(jq.archived);
        assert_eq!(jq.json.jq.as_deref(), Some(".[]"));
    }

    /// The checks gh runs before the token is read, in gh's order: the
    /// `--json` flags (plain errors) before `invalid limit: 0` (a flag
    /// error, so a `UsageError` that the usage block follows).
    #[test]
    fn prevalidate_checks_json_flags_then_the_limit() {
        use crate::cli::UsageError;
        use crate::cmd::Prevalidate;

        assert!(parse(&[]).prevalidate(true).is_ok());
        assert!(parse(&["--json", "gid,owner.name", "--jq", "."])
            .prevalidate(false)
            .is_ok());

        let err = parse(&["--limit", "0"])
            .prevalidate(true)
            .expect_err("zero");
        assert_eq!(err.to_string(), "invalid limit: 0");
        let usage = err.downcast_ref::<UsageError>().expect("a usage error");
        assert_eq!(usage.path, vec!["project", "list"]);

        let err = parse(&["--json", "nope"])
            .prevalidate(true)
            .expect_err("field");
        assert!(
            err.to_string()
                .starts_with("Unknown JSON field: \"nope\"\nAvailable fields:\n  archived\n"),
            "{err}"
        );
        assert!(err.downcast_ref::<UsageError>().is_none());

        let err = parse(&["--jq", "."]).prevalidate(true).expect_err("jq");
        assert_eq!(
            err.to_string(),
            "cannot use `--jq` without specifying `--json`"
        );

        let err = parse(&["--web", "--json"])
            .prevalidate(true)
            .expect_err("web");
        assert_eq!(err.to_string(), "cannot use `--web` with `--json`");

        // gh's order: a bad field is reported before a bad limit.
        let err = parse(&["--json", "nope", "--limit", "0"])
            .prevalidate(true)
            .expect_err("both");
        assert!(err.to_string().starts_with("Unknown JSON field"), "{err}");
    }

    /// `JSON_FIELDS` is exactly what a project off the wire has at its top
    /// level: `gid` and every name in `PROJECT_FIELDS`, the dotted ones by
    /// their parent, so `--json` can name any key it prints and no other.
    #[test]
    fn json_fields_cover_the_fields_we_request() {
        let mut expected: Vec<&str> = vec!["gid"];
        for f in PROJECT_FIELDS.split(',') {
            let head = f.split('.').next().unwrap();
            if !expected.contains(&head) {
                expected.push(head);
            }
        }
        expected.sort_unstable();
        let mut ours: Vec<&str> = JSON_FIELDS.to_vec();
        ours.sort_unstable();
        assert_eq!(ours, expected);
        assert_eq!(<Args as crate::cmd::Prevalidate>::JSON_FIELDS, JSON_FIELDS);
    }

    /// `--json gid,name` keeps those keys of each project, in the shape
    /// gh's `--json number,title,state` prints (reference/gh/issue-list-json.txt);
    /// a dotted field keeps its parent's shape; `--jq` reduces further, a
    /// string result printed bare.
    #[test]
    fn json_output_keeps_the_fields_asked_for() {
        let items = vec![
            item(json!({
                "gid": "1213478466398849", "name": "PR", "notes": "",
                "privacy_setting": "private", "owner": {"gid": "9", "name": "Jane"},
                "members": [{"gid": "9", "resource_type": "user"}],
                "modified_at": "2026-09-10T08:02:28.980Z"
            })),
            item(json!({"gid": "2", "name": "Bare", "owner": null})),
        ];
        let raw = Value::Array(items.into_iter().map(|i| i.raw).collect());

        let args = parse(&["--json", "gid,name"]);
        assert_eq!(
            json::render(&raw, &args.json, JSON_FIELDS).unwrap(),
            "[\n  {\n    \"gid\": \"1213478466398849\",\n    \"name\": \"PR\"\n  },\n  {\n    \"gid\": \"2\",\n    \"name\": \"Bare\"\n  }\n]\n"
        );

        let args = parse(&["--json", "name,owner.name"]);
        assert_eq!(
            json::render(&raw, &args.json, JSON_FIELDS).unwrap(),
            "[\n  {\n    \"name\": \"PR\",\n    \"owner\": {\n      \"name\": \"Jane\"\n    }\n  },\n  {\n    \"name\": \"Bare\",\n    \"owner\": null\n  }\n]\n"
        );

        let args = parse(&["--json", "name", "--jq", ".[].name"]);
        assert_eq!(
            json::render(&raw, &args.json, JSON_FIELDS).unwrap(),
            "PR\nBare\n"
        );

        // Bare `--json`: everything, as it came off the wire.
        let args = parse(&["--json"]);
        let all = json::render(&raw, &args.json, JSON_FIELDS).unwrap();
        assert!(all.contains("\"resource_type\": \"user\""), "{all}");
        assert!(all.contains("\"privacy_setting\": \"private\""), "{all}");
    }

    /// The help prose is hand-wrapped short of 78 columns like gh's, says
    /// where the output formats are documented, and its examples put the
    /// `--json` field list last, where `--json` reads the next word.
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
            assert!(line.starts_with("$ asana project list"), "{line:?}");
            assert!(line.chars().count() <= 78, "wider than 78: {line}");
        }
        assert!(examples.contains("--json gid,name,permalink_url"));
        assert!(examples.contains("--jq '.[].name'"));
        // The flag rows read as gh's (reference/gh/repo-list-help.txt).
        let json = cmd.get_arguments().find(|a| a.get_id() == "json").unwrap();
        assert_eq!(json.get_value_names().unwrap(), ["fields"]);
        assert_eq!(
            json.get_help().unwrap().to_string(),
            "Output JSON with the specified fields"
        );
        let jq = cmd.get_arguments().find(|a| a.get_id() == "jq").unwrap();
        assert_eq!(jq.get_short(), Some('q'));
        assert_eq!(jq.get_value_names().unwrap(), ["expression"]);
    }
}
