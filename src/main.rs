//! `asana`: parse the command line, dispatch, and turn errors into gh-style
//! messages and exit codes (0 ok, 1 error, 2 cancelled, 4 auth).
//!
//! The wording of every error lives in `error.rs`; this file only decides
//! the order things happen in: clap first (so `--help`, typos and missing
//! arguments never need a token), then the cheap identifier and flag checks
//! (`prevalidate`, still without a token), then the token and workspace,
//! then the command itself.

mod api;
mod cli;
mod cmd;
mod config;
mod ctx;
mod error;
mod render;
mod resolve;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};

use clap::Parser;

use crate::cli::{AuthCmd, Cli, Commands, ProjectCmd, TaskCmd, WorkspaceCmd};
use crate::ctx::Ctx;
use crate::error::{Report, EXIT_ERROR, EXIT_OK};
use crate::render::color::should_color;
use crate::render::Palette;

fn terminal_width() -> usize {
    if let Some((terminal_size::Width(w), _)) = terminal_size::terminal_size() {
        if w > 0 {
            return w as usize;
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|w| *w > 0)
        .unwrap_or(80)
}

/// Rust ignores SIGPIPE and turns a closed pipe into a panic in `println!`
/// ("failed printing to stdout: Broken pipe"). Restore the default so
/// `asana task list | head -1` ends quietly, the way gh does.
#[cfg(unix)]
fn reset_sigpipe() {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    // SAFETY: `signal(2)` with the default disposition has no preconditions;
    // it is called once, before any other thread exists.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

fn main() {
    reset_sigpipe();
    let code = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map(|rt| rt.block_on(run()))
        .unwrap_or_else(|e| {
            eprintln!("could not start the async runtime: {e}");
            EXIT_ERROR
        });
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

/// Print a report on stderr and hand back its exit code.
fn emit(report: Report) -> i32 {
    eprint!("{}", report.stderr);
    let _ = std::io::stderr().flush();
    report.code
}

async fn run() -> i32 {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let is_tty = std::io::stdout().is_terminal();
    let no_color_flag = argv.iter().any(|a| a == "--no-color");
    let palette = Palette::new(should_color(is_tty, no_color_flag));
    // Errors go to stderr, so they are styled by stderr's own terminal-ness
    // (never by stdout's): `asana task list 2>err.log` keeps the log clean,
    // and `NO_COLOR`, `TERM=dumb` and `--no-color` switch it off as usual.
    let stderr_palette = Palette::new(should_color(std::io::stderr().is_terminal(), no_color_flag));

    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(err) => {
            let rendered = error::render_clap_error(&err, &argv, &palette);
            print!("{}", rendered.stdout);
            let _ = std::io::stdout().flush();
            eprint!("{}", rendered.stderr);
            return rendered.code;
        }
    };
    // The command as typed (`["task", "view"]`), so an argument error can
    // print that command's synopsis under itself.
    let path = cli::command_path(&cli::command(), &argv);

    let Some(command) = cli.command else {
        print!("{}", cli::render_help(&[], &palette));
        return EXIT_OK;
    };
    // gh's CanPrompt: the command may ask questions only when every stream
    // is a terminal. Usage checks run before the token is read, so a script
    // missing `--title` exits 1 whether or not it is authenticated.
    let interactive = is_tty && std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if let Err(err) = command.prevalidate(interactive) {
        return emit(error::report(&err, &path, None, &stderr_palette));
    }

    // Commands that manage the token itself must run without one.
    let command = match command {
        Commands::Auth(AuthCmd::Login(args)) => {
            return match cmd::auth_login::run_login(args, &stderr_palette).await {
                Ok(()) => EXIT_OK,
                Err(err) => emit(error::report(&err, &path, None, &stderr_palette)),
            }
        }
        Commands::Auth(AuthCmd::Logout(args)) => {
            return match cmd::auth_login::run_logout(args, &stderr_palette).await {
                Ok(()) => EXIT_OK,
                Err(err) => emit(error::report(&err, &path, None, &stderr_palette)),
            }
        }
        Commands::Auth(AuthCmd::Token(args)) => {
            return match cmd::auth_login::run_token(args, &stderr_palette).await {
                Ok(()) => EXIT_OK,
                Err(err) => emit(error::report(&err, &path, None, &stderr_palette)),
            }
        }
        other => other,
    };

    let config = match config::load(cli.workspace.clone()) {
        Ok(c) => c,
        Err(err) => return emit(error::report_config(&err)),
    };
    let base = std::env::var("ASANA_BASE_URL")
        .ok()
        .filter(|b| !b.trim().is_empty());
    let api = match base.as_deref().map_or_else(
        || api::Client::new(config.token.clone()),
        |b| api::Client::with_base(b, config.token.clone()),
    ) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("could not create the HTTP client: {err}");
            return EXIT_ERROR;
        }
    };
    let ctx = Ctx::new(
        api,
        config,
        should_color(is_tty, cli.no_color),
        terminal_width(),
        is_tty,
        command.wants_json(),
    );

    match dispatch(&ctx, command).await {
        Ok(()) => EXIT_OK,
        Err(err) => emit(error::report(
            &err,
            &path,
            Some(&ctx.config.token_source),
            &stderr_palette,
        )),
    }
}

async fn dispatch(ctx: &Ctx, command: Commands) -> anyhow::Result<()> {
    match command {
        Commands::Auth(AuthCmd::Status(args)) => cmd::auth_status::run(ctx, args).await,
        Commands::Auth(AuthCmd::Login(_) | AuthCmd::Logout(_) | AuthCmd::Token(_)) => {
            unreachable!("handled before the token is loaded")
        }
        Commands::Workspace(WorkspaceCmd::List(args)) => cmd::workspace_list::run(ctx, args).await,
        Commands::Project(ProjectCmd::List(args)) => cmd::project_list::run(ctx, args).await,
        Commands::Project(ProjectCmd::View(args)) => cmd::project_view::run(ctx, args).await,
        Commands::Task(TaskCmd::List(args)) => cmd::task_list::run(ctx, args).await,
        Commands::Task(TaskCmd::View(args)) => cmd::task_view::run(ctx, args).await,
        Commands::Task(TaskCmd::Create(args)) => cmd::task_create::run(ctx, args).await,
        Commands::Task(TaskCmd::Complete(args)) => cmd::task_complete::run(ctx, args).await,
        Commands::Task(TaskCmd::Reopen(args)) => cmd::task_complete::run_reopen(ctx, args).await,
        Commands::Task(TaskCmd::Assign(args)) => cmd::task_assign::run(ctx, args).await,
        Commands::Task(TaskCmd::Comment(args)) => cmd::task_comment::run(ctx, args).await,
        Commands::Task(TaskCmd::Delete(args)) => cmd::task_delete::run(ctx, args).await,
    }
}
