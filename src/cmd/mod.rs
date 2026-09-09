//! One module per subcommand. Each exports `Args` (clap derive with all of
//! its help text) and `run(ctx, args)`. Shared little helpers live here.

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

use std::io::{BufRead, IsTerminal, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde::Serialize;

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

/// A stderr spinner while the API is busy, only on a terminal. It writes the
/// same byte pattern gh's does (`\r\x1b[K` + a braille frame) so nothing is
/// left behind once the escape sequences are stripped.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    lock: Arc<Mutex<()>>,
    handle: tokio::task::JoinHandle<()>,
}

const FRAMES: [&str; 10] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷", "⣾", "⣽"];

pub fn spinner(ctx: &Ctx) -> Option<Spinner> {
    if !ctx.is_tty || !std::io::stderr().is_terminal() {
        return None;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let lock = Arc::new(Mutex::new(()));
    let (stop2, lock2) = (Arc::clone(&stop), Arc::clone(&lock));
    let (tint, reset) = if ctx.color {
        ("\x1b[36m", "\x1b[0m")
    } else {
        ("", "")
    };
    let handle = tokio::spawn(async move {
        let mut i = 0usize;
        {
            let _guard = lock2.lock().unwrap_or_else(|e| e.into_inner());
            let mut err = std::io::stderr();
            let _ = write!(err, "\x1b[?25l");
            let _ = err.flush();
        }
        while !stop2.load(Ordering::Relaxed) {
            {
                let _guard = lock2.lock().unwrap_or_else(|e| e.into_inner());
                if stop2.load(Ordering::Relaxed) {
                    break;
                }
                let mut err = std::io::stderr();
                let _ = write!(err, "\r\x1b[K{tint}{}{reset}", FRAMES[i % FRAMES.len()]);
                let _ = err.flush();
            }
            i += 1;
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    });
    Some(Spinner { stop, lock, handle })
}

/// Stop and erase the spinner before printing anything.
pub fn finish(spinner: Option<Spinner>) {
    let Some(sp) = spinner else { return };
    sp.stop.store(true, Ordering::Relaxed);
    let _guard = sp.lock.lock().unwrap_or_else(|e| e.into_inner());
    sp.handle.abort();
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[K\x1b[?25h");
    let _ = err.flush();
}
