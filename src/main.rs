mod cli;
mod config;
mod context;
mod dependabot;
mod github;
mod log;
mod output;
mod pr;
mod prompt;
mod release;
mod run;
mod runners;
mod workflow;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};
use context::Ctx;
use github::Client;

fn main() -> Result<()> {
    // Exit quietly when stdout closes early, as when piped into head, instead of panicking.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();

    let ctx = Ctx {
        client: Client::new()?,
        repo_flag: cli.repo,
        json: cli.json,
        yes: cli.yes,
    };

    match cli.command {
        Command::Pr(cmd) => pr::run(&ctx, cmd),
        Command::Run(cmd) => run::run(&ctx, cmd),
        Command::Release(args) => release::run(&ctx, args),
        Command::Dependabot(cmd) => dependabot::run(&ctx, cmd),
        Command::Runners(args) => runners::run(&ctx, args),
        Command::Config(args) => config::run(&ctx, args),
    }
}
