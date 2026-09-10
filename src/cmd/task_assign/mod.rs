//! `asana task assign`: give a task to someone (or to nobody).
//!
//! Modelled on `gh issue edit --add-assignee`: the task's permalink is the
//! only thing printed on stdout, so it pipes, and the confirmation (`✓
//! Assigned task 1211234567890123 (Fix the login page) to Jane Doe`) goes
//! to stderr in the shape of gh's close/reopen lines. The two streams are
//! passed in explicitly, so the split is a tested property of `assign`
//! rather than a habit of the call site.
//!
//! One request is enough for `me` and for a user gid: Asana accepts both in
//! `PUT /tasks/{gid}` and answers an unknown one with `Not a valid actor
//! ID`, which reads back as `was not found`. An email or a name still costs
//! a lookup, because only that turns them into a user.

use std::io::Write;

use anyhow::Result;
use serde_json::{json, Value};

use crate::api::Task;
use crate::cli::usage_error;
use crate::cmd::{task_url, with_spinner, Prevalidate};
use crate::ctx::Ctx;
use crate::render::Palette;
use crate::resolve::{is_me, parse_task_id, resolve_assignee, IdKind, InvalidIdentifier};

/// Enough of the task back from the write to name it, name who has it, and
/// print its permalink; nothing else is used, so nothing else is asked for.
const FIELDS: &str = "name,assignee.name,permalink_url";

/// The command, for the usage block under a usage error.
const PATH: &[&str] = &["task", "assign"];

/// An assignee and `--unassign` ask for opposite things, in gh's words for
/// a pair that cannot both be given. The flag is named in backticks so the
/// block under the message shows its row and nothing else.
const BOTH_WAYS: &str = "specify only one of `--unassign` or an assignee";

#[derive(clap::Args, Debug)]
#[command(
    about = "Assign a task to someone",
    long_about = "Assign a task to a person in the workspace, or take it away from them.\n\n\
        A task has at most one assignee, so assigning it replaces whoever had it\n\
        before. Name an `<assignee>`, or pass `--unassign` to leave the task with\n\
        nobody.\n\n\
        An assignee can be an email address, a user gid, or a name; a name that\n\
        matches more than one person is an error that lists the candidates with\n\
        their emails and gids.\n\n\
        The `<assignee>` argument supports the following special values:\n\
        - `@me`: assign the task to yourself (`me` works too)",
    after_help = "# Assign a task to yourself\n\
        $ asana task assign 1211234567890123 \"@me\"\n\
        \n\
        # Assign a task to someone else, by email, by name or by user gid\n\
        $ asana task assign 1211234567890123 jane@example.com\n\
        $ asana task assign 1211234567890123 \"Jane Doe\"\n\
        $ asana task assign 1211234567890123 1200000000000001\n\
        \n\
        # Assign a task by URL\n\
        $ asana task assign https://app.asana.com/0/1200000000000000/1211234567890123 \"@me\"\n\
        \n\
        # Take a task away from whoever has it\n\
        $ asana task assign 1211234567890123 --unassign"
)]
pub struct Args {
    /// Task gid or URL
    #[arg(value_name = "task")]
    pub task: String,

    /// "@me", an email, a name or a user gid
    #[arg(value_name = "assignee", required_unless_present = "unassign")]
    pub assignee: Option<String>,

    /// Remove the assignee from the task
    #[arg(long)]
    pub unassign: bool,
}

/// Both checks cost neither a token nor a request, so they run before
/// either: a blank assignee can never name anyone (gh's `invalid issue
/// format: "abc"`, for the assignee), and an assignee beside `--unassign`
/// asks for two opposite things. clap could reject the pair itself, but it
/// writes the argument as `[assignee]`, brackets and all; the command owns
/// the sentence instead.
impl Prevalidate for Args {
    fn prevalidate(&self, _interactive: bool) -> Result<()> {
        self.wanted().map(|_| ())
    }
}

impl Args {
    /// Who the task should end up with: the assignee as typed with its
    /// surrounding space gone, or `None` for `--unassign`. The one place
    /// the two arguments are read together, so `prevalidate` and the run
    /// itself can never disagree about what was asked for.
    fn wanted(&self) -> Result<Option<&str>> {
        match (self.assignee.as_deref(), self.unassign) {
            (Some(_), true) => Err(usage_error(PATH, BOTH_WAYS)),
            (None, _) => Ok(None),
            (Some(given), false) => match given.trim() {
                "" => Err(InvalidIdentifier::new(IdKind::Assignee, given).into()),
                trimmed => Ok(Some(trimmed)),
            },
        }
    }
}

/// Digits and nothing else: an Asana gid.
fn is_gid(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// The `assignee` value to send: `null` for `--unassign`, `me` for `me` and
/// `@me`, a gid as it stands, and an email or a name resolved to the gid of
/// the one person it names.
async fn assignee_value(ctx: &Ctx, assignee: Option<&str>) -> Result<Value> {
    let Some(input) = assignee else {
        return Ok(Value::Null);
    };
    if is_me(input) {
        return Ok(Value::String("me".to_string()));
    }
    // A gid needs no lookup: Asana rejects one it does not know with
    // `assignee: Not a valid actor ID`, which is reported as not found.
    if is_gid(input) {
        return Ok(Value::String(input.to_string()));
    }
    Ok(Value::String(resolve_assignee(ctx, input).await?.gid))
}

/// `1211234567890123 (Fix the login page)`, the way gh writes
/// `cli/cli#13 (The title of the issue)`; the gid alone for an unnamed task.
fn label(task: &Task) -> String {
    match task
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(name) => format!("{} ({name})", task.gid),
        None => task.gid.clone(),
    }
}

/// The stderr confirmation: `✓ Assigned task <label> to <name>` with the
/// name Asana sent back, `✓ Unassigned task <label>` after `--unassign`.
fn status_line(p: &Palette, task: &Task, unassigned: bool) -> String {
    let label = label(task);
    if unassigned {
        return format!("{} Unassigned task {label}", p.success_icon());
    }
    match task
        .assignee
        .as_ref()
        .map(|a| a.display().trim())
        .filter(|n| !n.is_empty())
    {
        Some(name) => format!("{} Assigned task {label} to {name}", p.success_icon()),
        None => format!("{} Assigned task {label}", p.success_icon()),
    }
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    assign(ctx, &args, &mut std::io::stdout(), &mut std::io::stderr()).await
}

/// Write the assignee, then the status line to `err` and the permalink to
/// `out`. The callers hand in the real stdout and stderr (unlocked: the
/// spinner writes to stderr from another task, so a held lock would
/// deadlock it); tests hand in buffers.
async fn assign(ctx: &Ctx, args: &Args, out: &mut dyn Write, err: &mut dyn Write) -> Result<()> {
    let gid = parse_task_id(&args.task)?;
    let wanted = args.wanted()?;
    // Coloured by stderr's own terminal-ness, never stdout's: `2>log` keeps
    // the log clean and `| cat` keeps the tick green.
    let p = ctx.stderr_palette();
    let mut task: Task = with_spinner(ctx, "", async {
        let assignee = assignee_value(ctx, wanted).await?;
        let task: Task = ctx
            .api
            .put(
                &format!("/tasks/{gid}"),
                &[("opt_fields", FIELDS)],
                json!({ "assignee": assignee }),
            )
            .await?;
        Ok::<_, anyhow::Error>(task)
    })
    .await?;
    if task.gid.is_empty() {
        task.gid = gid;
    }
    writeln!(err, "{}", status_line(&p, &task, wanted.is_none()))?;
    writeln!(out, "{}", task_url(&task))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Named;
    use clap::error::ErrorKind;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        Harness::try_parse_from(std::iter::once("assign").chain(argv.iter().copied()))
            .map(|h| h.args)
    }

    fn task(name: Option<&str>, assignee: Option<&str>) -> Task {
        Task {
            gid: "1211234567890123".to_string(),
            name: name.map(str::to_string),
            assignee: assignee.map(|n| Named {
                gid: "1200000000000001".to_string(),
                name: Some(n.to_string()),
                email: None,
            }),
            ..Task::default()
        }
    }

    #[test]
    fn parses_assignee_or_unassign() {
        let a = parse(&["123", "me"]).unwrap();
        assert_eq!(a.task, "123");
        assert_eq!(a.assignee.as_deref(), Some("me"));
        assert!(!a.unassign);

        let a = parse(&["123", "--unassign"]).unwrap();
        assert_eq!(a.assignee, None);
        assert!(a.unassign);

        let a = parse(&["--unassign", "123"]).unwrap();
        assert_eq!(a.task, "123");
        assert!(a.unassign);
    }

    #[test]
    fn a_missing_assignee_is_a_missing_argument() {
        let err = parse(&["123"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
        let err = parse(&[]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    /// Both ways of saying who gets the task, given at once: one sentence
    /// in gh's `specify only one of ...` shape, raised before the token is
    /// read, naming the flag so the block under it shows that row alone.
    #[test]
    fn an_assignee_beside_unassign_is_a_usage_error() {
        let args = parse(&["123", "me", "--unassign"]).unwrap();
        let err = args.prevalidate(true).unwrap_err();
        assert_eq!(
            err.to_string(),
            "specify only one of `--unassign` or an assignee"
        );
        let usage = err
            .downcast_ref::<crate::cli::UsageError>()
            .expect("a usage error, so the usage block follows it");
        assert_eq!(usage.path, ["task", "assign"]);
    }

    /// A blank assignee is rejected before any request, with the shared
    /// typed error so `error.rs` prints gh's `invalid assignee: ""`.
    #[test]
    fn blank_assignee_is_an_invalid_identifier() {
        let args = parse(&["123", " "]).unwrap();
        let err = args.prevalidate(true).unwrap_err();
        assert_eq!(err.to_string(), "invalid assignee: \" \"");
        assert_eq!(
            err.downcast_ref::<InvalidIdentifier>(),
            Some(&InvalidIdentifier::new(IdKind::Assignee, " "))
        );
        assert!(parse(&["123", "me"]).unwrap().prevalidate(true).is_ok());
        assert!(parse(&["123", "--unassign"])
            .unwrap()
            .prevalidate(false)
            .is_ok());
    }

    /// The assignee is trimmed before it is used, so ` me ` is still `me`,
    /// and `--unassign` asks for nobody.
    #[test]
    fn wanted_trims_and_reads_unassign_as_nobody() {
        assert_eq!(
            parse(&["123", "  me "]).unwrap().wanted().unwrap(),
            Some("me")
        );
        assert_eq!(
            parse(&["123", "--unassign"]).unwrap().wanted().unwrap(),
            None
        );
        assert!(parse(&["123", "\t"]).unwrap().wanted().is_err());
    }

    #[test]
    fn gids_are_digits_only() {
        assert!(is_gid("1200000000000001"));
        assert!(!is_gid(""));
        assert!(!is_gid("12a"));
        assert!(!is_gid("jane@example.com"));
        assert!(!is_gid("Jane Doe"));
    }

    #[test]
    fn labels_read_like_gh() {
        assert_eq!(
            label(&task(Some("Fix the login page"), None)),
            "1211234567890123 (Fix the login page)"
        );
        assert_eq!(label(&task(Some("  "), None)), "1211234567890123");
        assert_eq!(label(&task(None, None)), "1211234567890123");
    }

    #[test]
    fn status_lines_name_who_got_the_task() {
        let p = Palette::off();
        assert_eq!(
            status_line(
                &p,
                &task(Some("Fix the login page"), Some("Jane Doe")),
                false
            ),
            "✓ Assigned task 1211234567890123 (Fix the login page) to Jane Doe"
        );
        assert_eq!(
            status_line(&p, &task(Some("Fix the login page"), None), false),
            "✓ Assigned task 1211234567890123 (Fix the login page)"
        );
        assert_eq!(
            status_line(&p, &task(Some("Fix the login page"), None), true),
            "✓ Unassigned task 1211234567890123 (Fix the login page)"
        );
    }

    #[test]
    fn the_tick_is_green_only_with_colour() {
        let line = status_line(&Palette::new(true), &task(None, Some("Jane Doe")), false);
        assert!(
            line.starts_with("\x1b[0;32m✓\x1b[0m Assigned task"),
            "{line:?}"
        );
        let line = status_line(&Palette::new(false), &task(None, Some("Jane Doe")), false);
        assert!(!line.contains('\x1b'), "{line:?}");
    }

    /// The description is hand-wrapped under 80 columns the way gh's are
    /// (the help renderer never rewraps a line that fits), it names the
    /// special value the argument takes, and it says that `--unassign`
    /// stands in for the argument, which is why the argument is optional.
    #[test]
    fn help_prose_is_hand_wrapped_like_ghs() {
        use clap::CommandFactory;
        let about = Harness::command().get_long_about().unwrap().to_string();
        for line in about.lines() {
            assert!(
                line.chars().count() <= crate::render::text::PROSE_WIDTH,
                "wider than gh's prose: {line:?}"
            );
            assert_eq!(line, line.trim_end(), "trailing space: {line:?}");
        }
        assert!(about.contains("- `@me`:"));
        assert!(about.contains("`--unassign`"));
    }

    /// gh's noun for the value of an assignee flag is `user`; ours is the
    /// positional's own name, and the flag table shows `--unassign` with no
    /// value at all.
    #[test]
    fn the_argument_placeholders_are_nouns() {
        use clap::CommandFactory;
        let cmd = Harness::command();
        let names = |id: &str| -> Vec<String> {
            cmd.get_arguments()
                .find(|a| a.get_id() == id)
                .unwrap()
                .get_value_names()
                .unwrap_or_default()
                .iter()
                .map(|s| s.to_string())
                .collect()
        };
        assert_eq!(names("task"), ["task"]);
        assert_eq!(names("assignee"), ["assignee"]);
        let unassign = cmd
            .get_arguments()
            .find(|a| a.get_id() == "unassign")
            .unwrap();
        assert!(
            matches!(unassign.get_action(), clap::ArgAction::SetTrue),
            "--unassign takes no value"
        );
    }

    // ---- The request and the stream split, against a canned Asana --------

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

    const ASSIGNED: &str = r#"{"data":{"gid":"12","name":"Fix the login page","permalink_url":"https://app.asana.com/1/9/project/8/task/12","assignee":{"gid":"7","name":"Jane Doe"}}}"#;
    const UNASSIGNED: &str = r#"{"data":{"gid":"12","name":"Fix the login page","permalink_url":"https://app.asana.com/1/9/project/8/task/12","assignee":null}}"#;
    const USER: &str = r#"{"data":{"gid":"7","name":"Jane Doe","email":"jane@example.com"}}"#;

    async fn assign_argv(
        argv: &[&str],
        bodies: Vec<&'static str>,
    ) -> (String, String, Vec<String>) {
        let (base, seen) = serve(bodies);
        let ctx = ctx_for(&base);
        let args = parse(argv).expect("parses");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assign(&ctx, &args, &mut out, &mut err)
            .await
            .expect("assign");
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            seen.join().unwrap(),
        )
    }

    /// `me` goes to Asana as it stands: one request, and the permalink is
    /// the only thing on stdout.
    #[tokio::test]
    async fn me_is_sent_without_a_lookup() {
        let (out, err, seen) = assign_argv(&["12", "me"], vec![ASSIGNED]).await;
        assert_eq!(out, "https://app.asana.com/1/9/project/8/task/12\n");
        assert_eq!(err, "✓ Assigned task 12 (Fix the login page) to Jane Doe\n");
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0],
            r#"PUT /tasks/12?opt_fields=name%2Cassignee.name%2Cpermalink_url HTTP/1.1 {"data":{"assignee":"me"}}"#
        );
        let (_, _, seen) = assign_argv(&["12", " @me "], vec![ASSIGNED]).await;
        assert!(
            seen[0].ends_with(r#"{"data":{"assignee":"me"}}"#),
            "{}",
            seen[0]
        );
    }

    /// A gid is a user identifier already, so it goes straight into the
    /// write instead of costing a `GET /users/<gid>` first.
    #[tokio::test]
    async fn a_gid_assignee_is_sent_straight_through() {
        let (_, err, seen) = assign_argv(&["12", "1200000000000001"], vec![ASSIGNED]).await;
        assert_eq!(err, "✓ Assigned task 12 (Fix the login page) to Jane Doe\n");
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].ends_with(r#"{"data":{"assignee":"1200000000000001"}}"#),
            "{}",
            seen[0]
        );
    }

    /// An email names a person only after a lookup, so it costs one, and
    /// the gid that comes back is what the write carries.
    #[tokio::test]
    async fn an_email_is_looked_up_first() {
        let (_, err, seen) = assign_argv(&["12", "jane@example.com"], vec![USER, ASSIGNED]).await;
        assert_eq!(err, "✓ Assigned task 12 (Fix the login page) to Jane Doe\n");
        assert_eq!(seen.len(), 2);
        assert!(
            seen[0].starts_with("GET /users/jane@example.com?opt_fields=name%2Cemail "),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].ends_with(r#"{"data":{"assignee":"7"}}"#),
            "{}",
            seen[1]
        );
    }

    /// `--unassign` writes a null assignee and says so.
    #[tokio::test]
    async fn unassign_writes_null() {
        let (out, err, seen) = assign_argv(&["12", "--unassign"], vec![UNASSIGNED]).await;
        assert_eq!(out, "https://app.asana.com/1/9/project/8/task/12\n");
        assert_eq!(err, "✓ Unassigned task 12 (Fix the login page)\n");
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].ends_with(r#"{"data":{"assignee":null}}"#),
            "{}",
            seen[0]
        );
    }

    /// A task URL is accepted wherever a gid is, and the gid in it is what
    /// the write addresses.
    #[tokio::test]
    async fn a_task_url_addresses_the_same_task() {
        let (_, _, seen) =
            assign_argv(&["https://app.asana.com/0/8/12/f", "me"], vec![ASSIGNED]).await;
        assert!(seen[0].starts_with("PUT /tasks/12?"), "{}", seen[0]);
    }
}
