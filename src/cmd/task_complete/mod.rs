//! `asana task complete` (alias `close`) and `asana task reopen`.
//!
//! One status line per task on stderr: a coloured glyph, the verb in the
//! past tense, the gid, then the title in parentheses (`✓ Completed task
//! 1211234567890123 (Fix the login page)`). A task already in the requested
//! state gets `! Task ... is already complete` and still exits 0, so the
//! command is safe to repeat.
//!
//! stdout is for data only: it stays empty unless `--comment` created a
//! story, whose permalink is printed alone so it pipes (`asana task complete
//! 123 --comment "done" | pbcopy` copies just the link). The two streams are
//! passed in explicitly, so the split is a tested property of `set_completed`
//! rather than a habit of the call sites.

use std::io::Write;

use anyhow::Result;
use serde_json::json;

use crate::api::{Story, Task};
use crate::cmd::with_spinner;
use crate::ctx::Ctx;
use crate::render::Palette;
use crate::resolve::parse_task_id;

/// Enough of the task to label it and know its state before we touch it.
const FIELDS: &str = "name,completed";

/// Asana sends the comment's own permalink when asked for it, so the link
/// we print is the one the web app would give, not a guess at its shape.
const STORY_FIELDS: &str = "permalink_url";

#[derive(clap::Args, Debug)]
#[command(
    about = "Mark tasks complete",
    long_about = "Mark one or more tasks complete.\n\n\
        A task that is already complete is reported and left alone, so the command is\n\
        safe to repeat. With `--comment` a closing comment is added to each task\n\
        first, and its link is printed on stdout; the status lines go to stderr, so\n\
        the link pipes cleanly.",
    after_help = "# Complete a task\n\
        $ asana task complete 1211234567890123\n\
        \n\
        # Complete a task and leave a closing comment\n\
        $ asana task complete 1211234567890123 --comment \"Shipped in 2.4\"\n\
        \n\
        # Complete several tasks at once\n\
        $ asana task complete 1211234567890123 1211234567890124\n\
        \n\
        # Complete a task by URL (`close` is an alias)\n\
        $ asana task close https://app.asana.com/0/1200000000000000/1211234567890123"
)]
pub struct CompleteArgs {
    /// Task gids or URLs
    #[arg(value_name = "task", required = true, num_args = 1..)]
    pub tasks: Vec<String>,

    /// Leave a closing comment
    #[arg(short = 'c', long, value_name = "string")]
    pub comment: Option<String>,
}

#[derive(clap::Args, Debug)]
#[command(
    about = "Reopen tasks",
    long_about = "Reopen one or more completed tasks.\n\n\
        A task that is already open is reported and left alone, so the command is\n\
        safe to repeat. With `--comment` a comment is added to each task first, and\n\
        its link is printed on stdout; the status lines go to stderr, so the link\n\
        pipes cleanly.",
    after_help = "# Reopen a task\n\
        $ asana task reopen 1211234567890123\n\
        \n\
        # Reopen a task and say why\n\
        $ asana task reopen 1211234567890123 --comment \"Regressed in 2.5\"\n\
        \n\
        # Reopen several tasks at once\n\
        $ asana task reopen 1211234567890123 1211234567890124\n\
        \n\
        # Reopen a task by URL\n\
        $ asana task reopen https://app.asana.com/0/1200000000000000/1211234567890123"
)]
pub struct ReopenArgs {
    /// Task gids or URLs
    #[arg(value_name = "task", required = true, num_args = 1..)]
    pub tasks: Vec<String>,

    /// Add a reopening comment
    #[arg(short = 'c', long, value_name = "string")]
    pub comment: Option<String>,
}

/// Nothing to check before the token is read (`cli.rs` already rejects bad
/// identifiers for every task command); see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for CompleteArgs {}

/// Nothing to check before the token is read; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for ReopenArgs {}

pub async fn run(ctx: &Ctx, args: CompleteArgs) -> Result<()> {
    set_completed(
        ctx,
        &args.tasks,
        args.comment.as_deref(),
        &COMPLETE,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
    .await
}

pub async fn run_reopen(ctx: &Ctx, args: ReopenArgs) -> Result<()> {
    set_completed(
        ctx,
        &args.tasks,
        args.comment.as_deref(),
        &REOPEN,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
    .await
}

/// What changes between `complete` and `reopen`: the state written, the
/// words on the status lines, and the colour of the tick.
struct Action {
    /// The `completed` value sent to the API.
    completed: bool,
    /// `Completed` / `Reopened`, for the success line.
    verb: &'static str,
    /// `complete` / `open`, for the already-in-that-state line.
    state: &'static str,
    /// Closing something paints the tick red, reopening it green.
    tick: fn(&Palette) -> String,
}

const COMPLETE: Action = Action {
    completed: true,
    verb: "Completed",
    state: "complete",
    tick: Palette::success_icon_red,
};

const REOPEN: Action = Action {
    completed: false,
    verb: "Reopened",
    state: "open",
    tick: Palette::success_icon,
};

/// What happened to one task.
enum Outcome {
    /// The state flipped; carries the comment permalink when one was added.
    Changed(Option<String>),
    /// Already in the requested state; nothing was written.
    Already,
}

/// Flip every task, writing the comment link (if any) to `out` and one
/// status line per task to `err`. The callers hand in the real stdout and
/// stderr (unlocked: the spinner writes to stderr from another task, so a
/// held lock would deadlock it); tests hand in buffers.
async fn set_completed(
    ctx: &Ctx,
    tasks: &[String],
    comment: Option<&str>,
    action: &Action,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    // Coloured by stderr's own terminal-ness, never stdout's: `2>log` keeps
    // the log clean and `| cat` keeps the tick green (or red).
    let p = ctx.stderr_palette();
    // `--comment ""` completes the task without commenting.
    let comment = comment.map(str::trim).filter(|c| !c.is_empty());
    // Reject every bad identifier before touching the API, so a typo in the
    // middle of a batch cannot half-complete it (cli.rs prevalidates too, so
    // this only matters for callers that skip it).
    let gids = tasks
        .iter()
        .map(|t| parse_task_id(t))
        .collect::<Result<Vec<_>>>()?;
    // One task at a time, stopping at the first error.
    for gid in &gids {
        let (task, outcome) = with_spinner(ctx, "", update(ctx, gid, comment, action)).await?;
        let label = label(gid, task.name.as_deref());
        match outcome {
            Outcome::Changed(link) => {
                // The comment's link first, then the status line: data on
                // stdout, status on stderr. Flushed here so the two arrive
                // in this order even under `2>&1 | ...`, where stdout is a
                // block-buffered pipe and stderr is not.
                if let Some(url) = link {
                    writeln!(out, "{url}")?;
                    out.flush()?;
                }
                writeln!(err, "{}", done_line(&p, action, &label))?;
            }
            Outcome::Already => writeln!(err, "{}", already_line(&p, action, &label))?,
        }
    }
    Ok(())
}

/// Fetch the task, bail out early if it is already in the requested state,
/// otherwise add the comment (if any) and flip `completed`.
async fn update(
    ctx: &Ctx,
    gid: &str,
    comment: Option<&str>,
    action: &Action,
) -> Result<(Task, Outcome)> {
    let task: Task = ctx
        .api
        .get(&format!("/tasks/{gid}"), &[("opt_fields", FIELDS)])
        .await?;
    if task.is_completed() == action.completed {
        return Ok((task, Outcome::Already));
    }
    let link = match comment {
        Some(text) => {
            let story: Story = ctx
                .api
                .post(
                    &format!("/tasks/{gid}/stories"),
                    &[("opt_fields", STORY_FIELDS)],
                    json!({ "text": text }),
                )
                .await?;
            Some(comment_link(gid, &story))
        }
        None => None,
    };
    let _updated: Task = ctx
        .api
        .put(
            &format!("/tasks/{gid}"),
            &[],
            json!({ "completed": action.completed }),
        )
        .await?;
    Ok((task, Outcome::Changed(link)))
}

/// The comment's permalink as Asana reports it, or the same shape Asana
/// uses (`/0/<task>/<task>/<story>`) built by hand when the response left
/// it out.
fn comment_link(task_gid: &str, story: &Story) -> String {
    story
        .permalink_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "https://{}/0/{task_gid}/{task_gid}/{}",
                crate::config::host(),
                story.gid
            )
        })
}

/// `1211234567890123 (Fix the login page)`: the identifier, then the title
/// in parentheses; the gid alone for an unnamed task.
fn label(gid: &str, name: Option<&str>) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => format!("{gid} ({name})"),
        None => gid.to_string(),
    }
}

/// `✓ Completed task 1211234567890123 (Fix the login page)`.
fn done_line(p: &Palette, action: &Action, label: &str) -> String {
    format!("{} {} task {label}", (action.tick)(p), action.verb)
}

/// `! Task 1211234567890123 (Fix the login page) is already complete`.
fn already_line(p: &Palette, action: &Action, label: &str) -> String {
    format!(
        "{} Task {label} is already {}",
        p.warning_icon(),
        action.state
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn label_puts_the_gid_first_and_the_title_in_parentheses() {
        assert_eq!(
            label("1211234567890123", Some("Fix the login page")),
            "1211234567890123 (Fix the login page)"
        );
        assert_eq!(
            label("1211234567890123", Some("  padded  ")),
            "1211234567890123 (padded)"
        );
        assert_eq!(label("1211234567890123", Some("")), "1211234567890123");
        assert_eq!(label("1211234567890123", None), "1211234567890123");
    }

    #[test]
    fn status_lines_name_the_verb_the_task_and_the_state() {
        let p = Palette::off();
        let label = "1211234567890123 (Fix the login page)";
        assert_eq!(
            done_line(&p, &COMPLETE, label),
            "✓ Completed task 1211234567890123 (Fix the login page)"
        );
        assert_eq!(
            done_line(&p, &REOPEN, label),
            "✓ Reopened task 1211234567890123 (Fix the login page)"
        );
        assert_eq!(
            already_line(&p, &COMPLETE, label),
            "! Task 1211234567890123 (Fix the login page) is already complete"
        );
        assert_eq!(
            already_line(&p, &REOPEN, label),
            "! Task 1211234567890123 (Fix the login page) is already open"
        );
    }

    /// A red tick for complete, a green one for reopen, a yellow bang for
    /// "already"; the words themselves are never painted.
    #[test]
    fn only_the_glyph_is_coloured() {
        let p = Palette::new(true);
        assert_eq!(
            done_line(&p, &COMPLETE, "1 (x)"),
            "\x1b[0;31m✓\x1b[0m Completed task 1 (x)"
        );
        assert_eq!(
            done_line(&p, &REOPEN, "1 (x)"),
            "\x1b[0;32m✓\x1b[0m Reopened task 1 (x)"
        );
        assert_eq!(
            already_line(&p, &COMPLETE, "1 (x)"),
            "\x1b[0;33m!\x1b[0m Task 1 (x) is already complete"
        );
    }

    /// The link is Asana's own permalink when the story carries one, and
    /// the same `/0/<task>/<task>/<story>` shape by hand when it does not.
    #[test]
    fn comment_link_prefers_asanas_permalink() {
        let story: Story = serde_json::from_str(
            r#"{"gid":"56","permalink_url":"https://app.asana.com/0/12/12/56"}"#,
        )
        .unwrap();
        assert_eq!(
            comment_link("12", &story),
            "https://app.asana.com/0/12/12/56"
        );
        let blank: Story = serde_json::from_str(r#"{"gid":"56","permalink_url":"  "}"#).unwrap();
        assert_eq!(
            comment_link("12", &blank),
            format!("https://{}/0/12/12/56", crate::config::host())
        );
        let none: Story = serde_json::from_str(r#"{"gid":"56"}"#).unwrap();
        assert_eq!(
            comment_link("12", &none),
            format!("https://{}/0/12/12/56", crate::config::host())
        );
    }

    #[derive(Parser)]
    struct Complete {
        #[command(flatten)]
        args: CompleteArgs,
    }

    #[derive(Parser)]
    struct Reopen {
        #[command(flatten)]
        args: ReopenArgs,
    }

    #[test]
    fn parses_many_tasks_and_a_comment() {
        let c = Complete::try_parse_from(["x", "1", "2", "-c", "done"]).unwrap();
        assert_eq!(c.args.tasks, vec!["1", "2"]);
        assert_eq!(c.args.comment.as_deref(), Some("done"));
        let c = Complete::try_parse_from(["x", "--comment", "done", "1"]).unwrap();
        assert_eq!(c.args.tasks, vec!["1"]);
        assert!(Complete::try_parse_from(["x"]).is_err());
        let r = Reopen::try_parse_from(["x", "https://app.asana.com/0/1/2"]).unwrap();
        assert_eq!(r.args.tasks, vec!["https://app.asana.com/0/1/2"]);
        assert!(r.args.comment.is_none());
    }

    /// The flag table row is `-c, --comment string`: the placeholder is the
    /// value's type, never its name.
    #[test]
    fn comment_placeholder_is_a_noun() {
        use clap::CommandFactory;
        for cmd in [Complete::command(), Reopen::command()] {
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_id() == "comment")
                .unwrap();
            assert_eq!(arg.get_short(), Some('c'));
            let names: Vec<_> = arg
                .get_value_names()
                .unwrap_or_default()
                .iter()
                .map(|s| s.to_string())
                .collect();
            assert_eq!(names, ["string"]);
        }
    }

    /// The description and the examples are hand-wrapped under 80 columns
    /// (the help renderer rewraps prose but prints examples verbatim, so a
    /// long one would run off a narrow terminal), no line has a trailing
    /// space, and the prose says which stream carries what.
    #[test]
    fn help_prose_and_examples_fit_eighty_columns() {
        use clap::CommandFactory;
        for cmd in [Complete::command(), Reopen::command()] {
            let about = cmd.get_long_about().unwrap().to_string();
            let examples = cmd.get_after_help().unwrap().to_string();
            for line in about.lines() {
                assert!(
                    line.chars().count() <= crate::render::text::PROSE_WIDTH,
                    "wider than the prose width: {line:?}"
                );
                assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
            }
            for line in examples.lines() {
                // Examples print two columns in, so they get two fewer.
                assert!(
                    line.chars().count() + 2 <= crate::render::text::PROSE_WIDTH,
                    "example runs past the screen: {line:?}"
                );
                assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
                assert!(
                    line.is_empty() || line.starts_with("# ") || line.starts_with("$ asana task "),
                    "not a comment or an invocation: {line:?}"
                );
            }
            assert!(about.contains("printed on stdout"), "{about}");
            assert!(about.contains("go to stderr"), "{about}");
        }
    }

    // ---- The stream split, end to end against a canned Asana -------------

    use std::io::Read;
    use std::net::TcpListener;

    /// A one-thread HTTP/1.1 server that answers each connection with the
    /// next canned body (all 200) and hands back the request lines it saw.
    fn serve(bodies: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut chunk).expect("read");
                    if n == 0 {
                        break buf.len();
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let content_length: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + content_length {
                    let n = stream.read(&mut chunk).expect("read body");
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let request_line = head.lines().next().unwrap_or("").to_string();
                let sent = String::from_utf8_lossy(&buf[head_end..]).to_string();
                seen.push(format!("{request_line} {sent}"));
                let out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=UTF-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(out.as_bytes()).expect("write");
                stream.flush().expect("flush");
            }
            seen
        });
        (base, handle)
    }

    /// A `Ctx` talking to `base` with stdout treated as a pipe (no spinner,
    /// no colour on stdout).
    fn ctx_for(base: &str) -> Ctx {
        let api = crate::api::Client::with_base(base, "tok").expect("client");
        let config = crate::config::Config {
            token: "tok".into(),
            token_source: crate::config::TokenSource::Env("ASANA_TOKEN"),
            workspace: None,
            config_path: std::path::PathBuf::from("/nonexistent/config.toml"),
        };
        Ctx::new(api, config, false, true, 80, false)
    }

    const OPEN_TASK: &str =
        r#"{"data":{"gid":"12","name":"Fix the login page","completed":false}}"#;
    const DONE_TASK: &str = r#"{"data":{"gid":"12","name":"Fix the login page","completed":true}}"#;
    const STORY: &str =
        r#"{"data":{"gid":"56","permalink_url":"https://app.asana.com/0/12/12/56"}}"#;

    /// Completing an open task: nothing on stdout, the ✓ line on stderr,
    /// and exactly one PUT with `completed: true`.
    #[tokio::test]
    async fn complete_writes_status_to_stderr_and_nothing_to_stdout() {
        let (base, seen) = serve(vec![OPEN_TASK, DONE_TASK]);
        let ctx = ctx_for(&base);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        set_completed(&ctx, &["12".into()], None, &COMPLETE, &mut out, &mut err)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "✓ Completed task 12 (Fix the login page)\n"
        );
        let seen = seen.join().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(
            seen[0].starts_with("GET /tasks/12?opt_fields=name%2Ccompleted "),
            "{}",
            seen[0]
        );
        assert_eq!(
            seen[1],
            r#"PUT /tasks/12 HTTP/1.1 {"data":{"completed":true}}"#
        );
    }

    /// A task already complete: the `!` line on stderr, stdout empty, no
    /// PUT at all, and still `Ok` (exit 0).
    #[tokio::test]
    async fn already_complete_writes_a_warning_to_stderr_and_no_put() {
        let (base, seen) = serve(vec![DONE_TASK]);
        let ctx = ctx_for(&base);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        set_completed(
            &ctx,
            &["12".into()],
            Some("late"),
            &COMPLETE,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "! Task 12 (Fix the login page) is already complete\n"
        );
        assert_eq!(seen.join().unwrap().len(), 1);
    }

    /// Reopening with `--comment`: the story is posted (asking for its
    /// permalink) before the flip, the link alone goes to stdout, and the
    /// ✓ line goes to stderr.
    #[tokio::test]
    async fn reopen_with_comment_prints_the_link_alone_on_stdout() {
        let (base, seen) = serve(vec![DONE_TASK, STORY, OPEN_TASK]);
        let ctx = ctx_for(&base);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        set_completed(
            &ctx,
            &["https://app.asana.com/0/1/12".into()],
            Some("  Regressed in 2.5  "),
            &REOPEN,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "https://app.asana.com/0/12/12/56\n"
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "✓ Reopened task 12 (Fix the login page)\n"
        );
        let seen = seen.join().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[1],
            r#"POST /tasks/12/stories?opt_fields=permalink_url HTTP/1.1 {"data":{"text":"Regressed in 2.5"}}"#
        );
        assert_eq!(
            seen[2],
            r#"PUT /tasks/12 HTTP/1.1 {"data":{"completed":false}}"#
        );
    }

    /// Several tasks in one command: one status line each, in the order
    /// they were given, whether the task moved or was already there, and
    /// stdout still carries nothing.
    #[tokio::test]
    async fn every_task_gets_its_own_status_line() {
        const OTHER_DONE: &str = r#"{"data":{"gid":"34","name":"Ship the docs","completed":true}}"#;
        let (base, seen) = serve(vec![OPEN_TASK, DONE_TASK, OTHER_DONE]);
        let ctx = ctx_for(&base);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        set_completed(
            &ctx,
            &["12".into(), "34".into()],
            None,
            &COMPLETE,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "✓ Completed task 12 (Fix the login page)\n\
             ! Task 34 (Ship the docs) is already complete\n"
        );
        let seen = seen.join().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[2].starts_with("GET /tasks/34?"), "{}", seen[2]);
    }

    /// A blank `--comment` completes the task without commenting.
    #[tokio::test]
    async fn blank_comment_posts_no_story() {
        let (base, seen) = serve(vec![OPEN_TASK, DONE_TASK]);
        let ctx = ctx_for(&base);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        set_completed(
            &ctx,
            &["12".into()],
            Some("   "),
            &COMPLETE,
            &mut out,
            &mut err,
        )
        .await
        .unwrap();
        assert!(out.is_empty());
        assert!(String::from_utf8(err)
            .unwrap()
            .starts_with("✓ Completed task 12"));
        let seen = seen.join().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].starts_with("PUT /tasks/12 "), "{}", seen[1]);
    }

    /// A bad identifier anywhere in the list is refused before any request
    /// is made, so a typo cannot half-complete a batch.
    #[tokio::test]
    async fn bad_identifier_touches_nothing() {
        let (base, seen) = serve(vec![]);
        let ctx = ctx_for(&base);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let e = set_completed(
            &ctx,
            &["12".into(), "abc".into()],
            None,
            &COMPLETE,
            &mut out,
            &mut err,
        )
        .await
        .unwrap_err();
        assert!(
            e.to_string().contains("invalid task format: \"abc\""),
            "{e}"
        );
        assert!(out.is_empty() && err.is_empty());
        assert!(seen.join().unwrap().is_empty());
    }
}
