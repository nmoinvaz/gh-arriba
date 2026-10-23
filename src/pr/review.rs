use anyhow::{Result, bail};

use crate::cli::ReviewCommand;
use crate::context::Ctx;

pub fn run(_ctx: &Ctx, _cmd: ReviewCommand) -> Result<()> {
    bail!("not implemented yet")
}
