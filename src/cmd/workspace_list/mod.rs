//! `asana workspace list`: the workspaces the token can reach.

use anyhow::Result;

use crate::cmd::{finish, print_json, spinner};
use crate::ctx::Ctx;
use crate::render::table::Cell;
use crate::render::{Palette, Table};

#[derive(clap::Args, Debug)]
#[command(
    about = "List the workspaces your token can reach",
    long_about = "List the workspaces your token can reach.\n\n\
        Pick the one commands use with `--workspace`, the ASANA_WORKSPACE environment\n\
        variable, or `workspace = \"...\"` in ~/.config/asana/config.toml.",
    after_help = "$ asana workspace list\n$ asana workspace list --json"
)]
pub struct Args {
    /// Output JSON instead of a table
    #[arg(long)]
    pub json: bool,
}

/// Nothing to check before the token is read; see `cmd::Prevalidate`.
impl crate::cmd::Prevalidate for Args {}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let sp = spinner(ctx);
    let me = ctx.me().await;
    finish(sp);
    let me = me?;
    if args.json {
        return print_json(&me.workspaces);
    }
    if me.workspaces.is_empty() {
        if ctx.is_tty {
            eprintln!("no workspaces are visible to this token");
        }
        return Ok(());
    }
    let p = Palette::new(ctx.color);
    let mut table = Table::new(&["NAME", "GID"], ctx.is_tty, ctx.width, p);
    for w in &me.workspaces {
        table.add_row(vec![
            Cell::colored(w.display(), |p, s| p.bold(s)),
            Cell::colored(w.gid.clone(), |p, s| p.muted(s)),
        ]);
    }
    print!("{}", table.render());
    Ok(())
}
