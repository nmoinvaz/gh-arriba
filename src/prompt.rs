use anyhow::{Result, bail};
use dialoguer::{Confirm, MultiSelect, Select};
use std::io::IsTerminal;

/// True when a person can answer prompts. Prompts render on stderr so stdout stays clean.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Ask a yes/no question. `yes` answers it up front, and without a terminal it is required.
pub fn confirm(prompt: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !interactive() {
        bail!("{prompt} Pass --yes to confirm without a prompt.");
    }
    Ok(Confirm::new().with_prompt(prompt).default(false).interact()?)
}

/// Pick one item. Fails without a terminal, so callers offer a flag for that case.
pub fn select(prompt: &str, items: &[String], default: usize) -> Result<usize> {
    if !interactive() {
        bail!("{prompt} needs a terminal.");
    }
    Ok(Select::new()
        .with_prompt(prompt)
        .items(items)
        .default(default)
        .interact()?)
}

/// Pick any number of items. Fails without a terminal, so callers offer a flag for that case.
pub fn multi_select(prompt: &str, items: &[String]) -> Result<Vec<usize>> {
    if !interactive() {
        bail!("{prompt} needs a terminal.");
    }
    Ok(MultiSelect::new().with_prompt(prompt).items(items).interact()?)
}
