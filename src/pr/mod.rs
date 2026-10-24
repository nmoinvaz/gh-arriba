mod apply;
mod checks;
mod comment;
mod dump;
mod failures;
mod labels;
pub mod model;
mod rebase;
mod retry;
mod review;
mod reviewers;
mod rollup;
mod strip;
mod threads;

use anyhow::Result;

use crate::cli::PrCommand;
use crate::context::Ctx;

pub fn run(ctx: &Ctx, cmd: PrCommand) -> Result<()> {
    match cmd {
        PrCommand::Threads(args) => threads::run_threads(ctx, args),
        PrCommand::Reply(args) => threads::run_reply(ctx, args),
        PrCommand::Resolve(args) => threads::run_resolve(ctx, args),
        PrCommand::Comment(cmd) => comment::run(ctx, cmd),
        PrCommand::Review(cmd) => review::run(ctx, cmd),
        PrCommand::Apply(args) => apply::run(ctx, args),
        PrCommand::Dump(args) => dump::run(ctx, args),
        PrCommand::Checks(args) => checks::run(ctx, args),
        PrCommand::Retry(args) => retry::run(ctx, args),
        PrCommand::Failures(args) => failures::run(ctx, args),
        PrCommand::Rebase(args) => rebase::run(ctx, args),
        PrCommand::Reviewers(args) => reviewers::run(ctx, args),
        PrCommand::Labels(args) => labels::run(ctx, args),
        PrCommand::Strip(args) => strip::run(ctx, args),
    }
}
