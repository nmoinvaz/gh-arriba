mod cli;
mod config;
mod context;
mod dependabot;
mod github;
mod output;
mod pr;
mod prompt;
mod release;
mod run;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};
use context::Ctx;
use github::Client;

fn main() -> Result<()> {
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
        Command::Config(args) => config::run(&ctx, args),
    }
}
