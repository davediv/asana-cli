//! `asana`: parse the command line, dispatch, and turn errors into gh-style
//! messages and exit codes (0 ok, 1 error, 2 cancelled, 4 auth).

mod api;
mod cli;
mod cmd;
mod config;
mod ctx;
mod render;
mod resolve;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};

use clap::Parser;

use crate::api::ApiError;
use crate::cli::{
    AuthCmd, Cancelled, Cli, Commands, ProjectCmd, SilentExit, TaskCmd, UsageError, WorkspaceCmd,
};
use crate::config::ConfigError;
use crate::ctx::Ctx;
use crate::render::color::should_color;
use crate::render::Palette;

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_CANCELLED: i32 = 2;
const EXIT_AUTH: i32 = 4;

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

fn main() {
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

async fn run() -> i32 {
    let argv: Vec<OsString> = std::env::args_os().collect();
    let is_tty = std::io::stdout().is_terminal();
    let no_color_flag = argv.iter().any(|a| a == "--no-color");
    let palette = Palette::new(should_color(is_tty, no_color_flag));

    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(err) => {
            let rendered = cli::render_clap_error(&err, &argv, &palette);
            print!("{}", rendered.stdout);
            eprint!("{}", rendered.stderr);
            return rendered.code;
        }
    };

    let Some(command) = cli.command else {
        print!("{}", cli::render_help(&[], &palette));
        return EXIT_OK;
    };
    if let Err(err) = command.prevalidate() {
        return report(&err);
    }

    let config = match config::load(cli.workspace.clone()) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("{err}");
            return match err {
                ConfigError::MissingToken(_) => EXIT_AUTH,
                _ => EXIT_ERROR,
            };
        }
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
        Err(err) => report(&err),
    }
}

async fn dispatch(ctx: &Ctx, command: Commands) -> anyhow::Result<()> {
    match command {
        Commands::Auth(AuthCmd::Status(args)) => cmd::auth_status::run(ctx, args).await,
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

/// Print a runtime error the way gh does: one or two lines, no usage dump
/// (except for usage errors), and pick the exit code.
fn report(err: &anyhow::Error) -> i32 {
    if let Some(usage) = err.downcast_ref::<UsageError>() {
        eprintln!("{}\n", usage.message);
        eprint!("{}", cli::usage_block(&usage.path));
        return EXIT_ERROR;
    }
    if err.downcast_ref::<Cancelled>().is_some() {
        return EXIT_CANCELLED;
    }
    if let Some(silent) = err.downcast_ref::<SilentExit>() {
        return silent.0;
    }
    if let Some(api) = err.downcast_ref::<ApiError>() {
        return report_api(api);
    }
    if let Some(cfg) = err.downcast_ref::<ConfigError>() {
        eprintln!("{cfg}");
        return match cfg {
            ConfigError::MissingToken(_) => EXIT_AUTH,
            _ => EXIT_ERROR,
        };
    }
    eprintln!("{err:#}");
    EXIT_ERROR
}

fn report_api(err: &ApiError) -> i32 {
    match err {
        ApiError::Unauthorized { .. } => {
            eprintln!("{err}");
            eprintln!(
                "Try setting {} to a valid personal access token from {}",
                config::TOKEN_ENV,
                config::PAT_URL
            );
            EXIT_AUTH
        }
        ApiError::Network(source) => {
            eprintln!(
                "error connecting to app.asana.com: {}",
                network_reason(source)
            );
            eprintln!("check your internet connection or https://status.asana.com");
            EXIT_ERROR
        }
        other => {
            eprintln!("{other}");
            EXIT_ERROR
        }
    }
}

/// The innermost reason of a reqwest error, without the "error sending request for url" wrapper.
fn network_reason(err: &reqwest::Error) -> String {
    let mut cur: &dyn std::error::Error = err;
    while let Some(next) = cur.source() {
        cur = next;
    }
    if err.is_timeout() {
        "timed out".to_string()
    } else {
        cur.to_string()
    }
}
