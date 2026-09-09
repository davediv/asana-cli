//! One module per subcommand. Each exports `Args` (clap derive with all of
//! its help text, implementing `Prevalidate`) and `run(ctx, args)`. Shared
//! little helpers live here: the spinner, prompts, browser, JSON, permalinks.

pub mod auth_status;
pub mod help;
pub mod project_list;
pub mod project_view;
pub mod task_assign;
pub mod task_comment;
pub mod task_complete;
pub mod task_create;
pub mod task_delete;
pub mod task_list;
pub mod task_view;
pub mod workspace_list;

use std::future::Future;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::Serialize;
use url::{Position, Url};

use crate::api::Task;
use crate::ctx::Ctx;
use crate::render::text::display_url;

/// Open a URL with the system browser and say so on stderr, like gh.
pub fn open_in_browser(url: &str) -> Result<()> {
    eprintln!("Opening {} in your browser.", display_url(url));
    open::that_detached(url).with_context(|| format!("could not open {url}"))?;
    Ok(())
}

/// Pretty JSON on stdout, no colour.
pub fn print_json<T: Serialize>(value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value).context("could not serialise to JSON")?;
    println!("{text}");
    Ok(())
}

/// Read a `--body-file` / `--notes-file` argument; `-` means standard input.
pub fn read_text_file(path: &str) -> Result<String> {
    if path == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("could not read standard input")?;
        return Ok(buf);
    }
    std::fs::read_to_string(path).with_context(|| format!("could not read {path}"))
}

/// gh prompts only when stdin, stdout and stderr are all terminals.
pub fn can_prompt(ctx: &Ctx) -> bool {
    ctx.is_tty && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Ask a one-line question on stderr and read the answer from stdin.
pub fn prompt_line(question: &str) -> Result<String> {
    eprint!("? {question} ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("could not read standard input")?;
    Ok(line.trim().to_string())
}

/// `? question (y/N)` on stderr; anything but y/yes is a no.
pub fn confirm(question: &str) -> Result<bool> {
    let answer = prompt_line(&format!("{question} (y/N)"))?;
    Ok(matches!(answer.to_lowercase().as_str(), "y" | "yes"))
}

/// Checks a command can run before the token is read: identifier formats,
/// flag combinations, and gh's "must provide `--title` ... when not running
/// interactively". `main` calls it before `config::load`, so a usage error
/// exits 1 even when there is no token (gh checks flags before auth).
///
/// Return `cli::usage_error(PATH, ..)` for the message + `Usage:` block, or
/// any other error for a bare one-line message; both exit 1. `interactive`
/// is gh's `CanPrompt`: stdin, stdout and stderr are all terminals.
pub trait Prevalidate {
    fn prevalidate(&self, interactive: bool) -> Result<()> {
        let _ = interactive;
        Ok(())
    }
}

/// The task's permalink, or a URL Asana redirects to the task when the API
/// did not send one (`https://app.asana.com/0/0/<gid>/f`).
#[allow(dead_code)]
pub fn task_url(task: &Task) -> String {
    task.permalink_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://app.asana.com/0/0/{}/f", task.gid))
}

/// The permalink of a comment on `task`, built from the task's permalink the
/// way Asana's own "Copy comment link" does:
///
/// - V0 `https://app.asana.com/0/<project>/<task>[/f]`
///   becomes `https://app.asana.com/0/<project>/<task>/<story>/f`
/// - V1 `https://app.asana.com/1/<workspace>[/project/<p>]/task/<task>[?focus=true]`
///   becomes `https://app.asana.com/1/<workspace>[/project/<p>]/task/<task>/comment/<story>`
///
/// Anything unrecognised falls back to the project-less V0 form.
#[allow(dead_code)]
pub fn comment_url(task: &Task, story_gid: &str) -> String {
    comment_url_from(&task_url(task), &task.gid, story_gid)
}

/// `comment_url` for callers that already have the task's permalink.
#[allow(dead_code)]
pub fn comment_url_from(task_url: &str, task_gid: &str, story_gid: &str) -> String {
    let fallback = || format!("https://app.asana.com/0/0/{task_gid}/{story_gid}/f");
    let Ok(url) = Url::parse(task_url) else {
        return fallback();
    };
    if !matches!(url.scheme(), "http" | "https") {
        return fallback();
    }
    let origin = &url[..Position::BeforePath];
    let segs: Vec<&str> = url
        .path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let is_gid = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match segs.as_slice() {
        ["0", project, task, ..] if is_gid(project) && is_gid(task) => {
            format!("{origin}/0/{project}/{task}/{story_gid}/f")
        }
        ["1", ..] => {
            let task_at = segs
                .iter()
                .position(|s| *s == "task")
                .map(|pos| pos + 1)
                .filter(|i| segs.get(*i).is_some_and(|t| is_gid(t)));
            match task_at {
                Some(i) => format!("{origin}/{}/comment/{story_gid}", segs[..=i].join("/")),
                None => fallback(),
            }
        }
        _ => fallback(),
    }
}

/// A stderr spinner while the API is busy, only on a terminal. It writes the
/// same bytes gh's does (`\x1b[?25l`, then `\r\x1b[K\r` + a cyan braille frame
/// per tick, then `\x1b[?25h\r\x1b[K`), and nothing at all until the request
/// has taken `SPINNER_DELAY`, so a fast request leaves no trace in a
/// transcript, exactly like gh's `issue list`.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    /// Set once the first frame has been written, so `finish` knows whether
    /// there is anything to erase.
    drawn: Arc<AtomicBool>,
    lock: Arc<Mutex<()>>,
    handle: tokio::task::JoinHandle<()>,
}

const FRAMES: [&str; 10] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷", "⣾", "⣽"];
/// How long a request may take before the spinner shows.
pub const SPINNER_DELAY: Duration = Duration::from_millis(100);
const SPINNER_TICK: Duration = Duration::from_millis(80);

pub fn spinner(ctx: &Ctx) -> Option<Spinner> {
    spinner_with_label(ctx, "")
}

/// A spinner followed by ` label`, like gh's `StartProgressIndicatorWithLabel`.
pub fn spinner_with_label(ctx: &Ctx, label: &str) -> Option<Spinner> {
    if !ctx.is_tty || !std::io::stderr().is_terminal() {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let drawn = Arc::new(AtomicBool::new(false));
    let lock = Arc::new(Mutex::new(()));
    let (stop2, drawn2, lock2) = (Arc::clone(&stop), Arc::clone(&drawn), Arc::clone(&lock));
    let (tint, reset) = if ctx.color {
        ("\x1b[36m", "\x1b[0m")
    } else {
        ("", "")
    };
    let suffix = if label.is_empty() {
        String::new()
    } else {
        format!(" {label}")
    };
    let handle = tokio::spawn(async move {
        tokio::time::sleep(SPINNER_DELAY).await;
        let mut i = 0usize;
        while !stop2.load(Ordering::Relaxed) {
            {
                let _guard = lock2.lock().unwrap_or_else(|e| e.into_inner());
                if stop2.load(Ordering::Relaxed) {
                    break;
                }
                let mut err = std::io::stderr();
                if !drawn2.swap(true, Ordering::Relaxed) {
                    let _ = write!(err, "\x1b[?25l");
                }
                let _ = write!(
                    err,
                    "\r\x1b[K\r{tint}{}{reset}{suffix}",
                    FRAMES[i % FRAMES.len()]
                );
                let _ = err.flush();
            }
            i += 1;
            tokio::time::sleep(SPINNER_TICK).await;
        }
    });
    Some(Spinner {
        stop,
        drawn,
        lock,
        handle,
    })
}

/// Stop and erase the spinner before printing anything. Writes nothing when
/// no frame was ever drawn.
pub fn finish(spinner: Option<Spinner>) {
    let Some(sp) = spinner else { return };
    sp.stop.store(true, Ordering::Relaxed);
    let _guard = sp.lock.lock().unwrap_or_else(|e| e.into_inner());
    sp.handle.abort();
    if sp.drawn.load(Ordering::Relaxed) {
        let mut err = std::io::stderr();
        let _ = write!(err, "\x1b[?25h\r\x1b[K");
        let _ = err.flush();
    }
}

/// Run `call` behind the spinner (with ` message` after the frame when it is
/// not empty) and stop the spinner before returning, on success or error,
/// so the result can be printed on a clean line.
#[allow(dead_code)]
pub async fn with_spinner<T, E>(
    ctx: &Ctx,
    message: &str,
    call: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let sp = spinner_with_label(ctx, message);
    let out = call.await;
    finish(sp);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(url: Option<&str>) -> Task {
        Task {
            gid: "34".to_string(),
            permalink_url: url.map(str::to_string),
            ..Task::default()
        }
    }

    #[test]
    fn task_url_falls_back_when_missing() {
        assert_eq!(task_url(&task(None)), "https://app.asana.com/0/0/34/f");
        assert_eq!(
            task_url(&task(Some("  "))),
            "https://app.asana.com/0/0/34/f"
        );
        assert_eq!(
            task_url(&task(Some("https://app.asana.com/1/11/task/34"))),
            "https://app.asana.com/1/11/task/34"
        );
    }

    #[test]
    fn v0_comment_urls_end_in_story_and_f() {
        assert_eq!(
            comment_url(&task(Some("https://app.asana.com/0/12/34")), "56"),
            "https://app.asana.com/0/12/34/56/f"
        );
        assert_eq!(
            comment_url(&task(Some("https://app.asana.com/0/12/34/f")), "56"),
            "https://app.asana.com/0/12/34/56/f"
        );
        assert_eq!(
            comment_url(&task(None), "56"),
            "https://app.asana.com/0/0/34/56/f"
        );
    }

    #[test]
    fn v1_comment_urls_use_the_comment_segment() {
        for (url, want) in [
            (
                "https://app.asana.com/1/11/task/34",
                "https://app.asana.com/1/11/task/34/comment/56",
            ),
            (
                "https://app.asana.com/1/11/project/12/task/34",
                "https://app.asana.com/1/11/project/12/task/34/comment/56",
            ),
            (
                "https://app.asana.com/1/11/project/12/task/34?focus=true",
                "https://app.asana.com/1/11/project/12/task/34/comment/56",
            ),
            (
                "https://app.asana.com/1/11/task/34/f",
                "https://app.asana.com/1/11/task/34/comment/56",
            ),
        ] {
            assert_eq!(comment_url_from(url, "34", "56"), want, "input {url:?}");
        }
    }

    #[test]
    fn unrecognised_permalinks_fall_back_to_v0() {
        for junk in [
            "",
            "not a url",
            "ftp://app.asana.com/0/12/34",
            "https://app.asana.com/1/11/project/12",
            "https://app.asana.com/2/11/task/34",
            "https://app.asana.com/0/abc/34",
        ] {
            assert_eq!(
                comment_url_from(junk, "34", "56"),
                "https://app.asana.com/0/0/34/56/f",
                "input {junk:?}"
            );
        }
    }
}
