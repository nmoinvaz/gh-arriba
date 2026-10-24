mod apply;
mod dump;
mod failures;
pub mod model;
mod rebase;
mod retry;
mod review;
mod reviewers;
mod threads;

use anyhow::Result;

use crate::cli::PrCommand;
use crate::context::Ctx;

pub fn run(ctx: &Ctx, cmd: PrCommand) -> Result<()> {
    match cmd {
        PrCommand::Threads(args) => threads::run_threads(ctx, args),
        PrCommand::Reply(args) => threads::run_reply(ctx, args),
        PrCommand::Resolve(args) => threads::run_resolve(ctx, args),
        PrCommand::Review(cmd) => review::run(ctx, cmd),
        PrCommand::Apply(args) => apply::run(ctx, args),
        PrCommand::Dump(args) => dump::run(ctx, args),
        PrCommand::Retry(args) => retry::run(ctx, args),
        PrCommand::Failures(args) => failures::run(ctx, args),
        PrCommand::Rebase(args) => rebase::run(ctx, args),
        PrCommand::Reviewers(args) => reviewers::run(ctx, args),
    }
}
