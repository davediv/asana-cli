//! `asana auth status`: who the token belongs to and where it came from.

use anyhow::Result;

use crate::api::ApiError;
use crate::cli::SilentExit;
use crate::cmd::{finish, spinner};
use crate::config::mask_token;
use crate::ctx::Ctx;
use crate::render::Palette;

#[derive(clap::Args, Debug)]
#[command(
    about = "Display the account and workspace the token is for",
    long_about = "Display the account and workspace the token is for.\n\n\
        Verifies the token against Asana, and reports which environment variable\n\
        or config file it was read from and which workspace commands will use.",
    after_help = "$ asana auth status"
)]
pub struct Args {
    /// Display the full token instead of masking it
    #[arg(short = 't', long)]
    pub show_token: bool,
}

pub async fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let p = Palette::new(ctx.color);
    let sp = spinner(ctx);
    let me = ctx.me().await;
    let token = if args.show_token {
        ctx.config.token.clone()
    } else {
        mask_token(&ctx.config.token)
    };
    let source = ctx.config.token_source.to_string();
    match me {
        Ok(me) => {
            let ws = ctx.workspace().await;
            finish(sp);
            println!("{}", p.bold("app.asana.com"));
            let who = match &me.email {
                Some(email) => format!("{} ({email})", p.bold(me.display())),
                None => p.bold(me.display()),
            };
            println!("  {} Logged in to app.asana.com as {who}", p.success_icon());
            println!("  - Token source: {}", p.bold(&source));
            println!("  - Token: {}", p.bold(&token));
            match ws {
                Ok(ws) => println!("  - Workspace: {} ({})", p.bold(ws.display()), ws.gid),
                Err(err) => {
                    let first = err.to_string();
                    let first = first.lines().next().unwrap_or("").to_string();
                    println!("  - Workspace: {} {first}", p.warning_icon());
                    if me.workspaces.len() > 1 {
                        for w in &me.workspaces {
                            println!("      {} ({})", w.display(), w.gid);
                        }
                    }
                }
            }
            Ok(())
        }
        Err(err) => {
            finish(sp);
            if let Some(ApiError::Unauthorized { .. }) = err.downcast_ref::<ApiError>() {
                println!("{}", p.bold("app.asana.com"));
                println!(
                    "  {} Failed to log in to app.asana.com using the token from the {source}",
                    p.failure_icon()
                );
                println!("  - The token from the {source} is invalid.");
                println!("  - Token: {}", p.bold(&token));
                return Err(SilentExit(4).into());
            }
            Err(err)
        }
    }
}
