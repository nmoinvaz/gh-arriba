use anyhow::{Result, bail};

use crate::cli::DumpArgs;
use crate::context::Ctx;

pub fn run(_ctx: &Ctx, _args: DumpArgs) -> Result<()> {
    bail!("not implemented yet")
}
