use anyhow::{Result, bail};

use crate::cli::RunCommand;
use crate::context::Ctx;

pub fn run(_ctx: &Ctx, _cmd: RunCommand) -> Result<()> {
    bail!("not implemented yet")
}
