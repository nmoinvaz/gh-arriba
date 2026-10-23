use anyhow::{Result, bail};

use crate::cli::{ReplyArgs, ResolveArgs, ThreadsArgs};
use crate::context::Ctx;

pub fn run_threads(_ctx: &Ctx, _args: ThreadsArgs) -> Result<()> {
    bail!("not implemented yet")
}

pub fn run_reply(_ctx: &Ctx, _args: ReplyArgs) -> Result<()> {
    bail!("not implemented yet")
}

pub fn run_resolve(_ctx: &Ctx, _args: ResolveArgs) -> Result<()> {
    bail!("not implemented yet")
}
