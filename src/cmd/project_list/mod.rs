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
//! first, and the widest column gives way to `...` first. Piped: `gid  name
//! description  info  modified` separated by tabs, no header, ISO-8601 time.
//! The table itself is the shared `render::Table`, which pads, truncates and
//! paints in the bytes gh emits; the tests at the bottom hold it to the
//! reference transcript.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::api::{Project, PROJECT_FIELDS};
use crate::cmd::{open_in_browser, print_json, with_spinner};
use crate::ctx::Ctx;
use crate::render::table::Cell;
use crate::render::time::{fuzzy_ago, iso, now};
use crate::render::{Palette, Table};

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
        on hold, complete) and whether it is archived.\n\n\
        A name from the NAME column is accepted by every command that takes a\n\
        project, for example `asana task list --project \"Roadmap\"`; a\n\
        case-insensitive unique prefix is enough.\n\n\
        For more information about output formatting, see `asana help formatting`.",
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

    /// Maximum number of projects to list
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

/// The terminal table: gh's `NAME  DESCRIPTION  INFO  UPDATED`, laid out
/// for `width` columns and painted with `palette`; UPDATED is relative to
/// `now`.
fn terminal_table(items: &[Item], width: usize, palette: Palette, now: DateTime<Utc>) -> Table {
    let mut table = Table::new(
        &["NAME", "DESCRIPTION", "INFO", "UPDATED"],
        true,
        width,
        palette,
    );
    for item in items {
        let p = &item.project;
        let updated = updated_at(p).map(|t| fuzzy_ago(now, t)).unwrap_or_default();
        table.add_row(vec![
            Cell::colored(name(p), Palette::bold),
            Cell::plain(description(p)),
            Cell::colored(info(p), info_color(p)),
            Cell::colored(updated, Palette::muted),
        ]);
    }
    table
}

/// The piped table: `gid  name  description  info  modified` per line,
/// tab-separated, no header, ISO-8601 time, nothing truncated.
fn piped_table(items: &[Item]) -> Table {
    let mut table = Table::new(&[], false, usize::MAX, Palette::new(false));
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

/// Nothing to check before the token is read; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for Args {}

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

    if args.json {
        let values: Vec<Value> = items.into_iter().map(|i| i.raw).collect();
        return print_json(&values);
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
            item(json!({"gid": "old", "modified_at": "2026-09-01T00:00:00.000Z"})),
            item(json!({"gid": "new", "created_at": "2026-09-09T00:00:00.000Z"})),
            item(json!({"gid": "none"})),
        ];
        sort_newest_first(&mut items);
        let order: Vec<&str> = items.iter().map(|i| i.project.gid.as_str()).collect();
        assert_eq!(order, vec!["new", "old", "none"]);
        assert_eq!(
            piped_table(&items).render(),
            "new\tnew\t\t\t2026-09-09T00:00:00Z\nold\told\t\t\t2026-09-01T00:00:00Z\nnone\tnone\t\t\t\n"
        );
        let out =
            terminal_table(&items, 120, Palette::new(true), at("2026-09-10T00:00:00Z")).render();
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[1].ends_with("\x1b[38;5;242mabout 1 day ago\x1b[0m"),
            "{:?}",
            lines[1]
        );
        // an empty INFO is still padded and painted, as gh paints after
        // padding; an unknown time in the last column is nothing at all
        assert_eq!(
            lines[3],
            "\x1b[0;1;39mnone\x1b[0m               \x1b[38;5;242m    \x1b[0m  "
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
        let mut t = Table::new(
            &["NAME", "DESCRIPTION", "INFO", "UPDATED"],
            true,
            120,
            Palette::new(color),
        );
        for (name, desc, info, updated) in rows {
            t.add_row(vec![
                Cell::colored(name, Palette::bold),
                Cell::plain(desc),
                Cell::colored(info, Palette::muted),
                Cell::colored(updated, Palette::muted),
            ]);
        }
        t
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
    /// cells a terminal draws them in, and the empty DESCRIPTION column must
    /// take only its header's width, not a flexible share.
    #[test]
    fn emoji_names_align_and_empty_descriptions_take_no_room() {
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
        // NAME is 8 wide (TEMPLATE), DESCRIPTION 11 (its header), INFO 7,
        // UPDATED 20 (`about 57 minutes ago`).
        assert_eq!(
            lines,
            vec![
                "NAME      DESCRIPTION  INFO     UPDATED             ",
                "PR                     private  about 57 minutes ago",
                "\u{2699}\u{FE0F} UTL                 private  about 17 hours ago",
                "\u{1F47E} FUN                 private  about 21 hours ago",
                "\u{1F468}\u{200D}\u{2696}\u{FE0F} LEGAL               private  about 22 hours ago",
                "TEMPLATE               private  about 3 months ago",
            ]
        );
        for line in &lines[1..] {
            let info_at = line.find("private").unwrap();
            assert_eq!(width(&line[..info_at]), 8 + 2 + 11 + 2, "{line:?}");
        }
        assert_eq!(width("\u{2699}\u{FE0F} UTL"), 6);
        assert_eq!(width("\u{1F47E} FUN"), 6);
        assert_eq!(width("\u{1F468}\u{200D}\u{2696}\u{FE0F} LEGAL"), 8);
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
}
