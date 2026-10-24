use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, MultiSelect, Select};
use std::io::{IsTerminal, Read};

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
    Ok(Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()?)
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
    Ok(MultiSelect::new()
        .with_prompt(prompt)
        .items(items)
        .interact()?)
}

/// Text from the argument, or from stdin for `-` or a piped stdin. `what` names it in errors.
pub fn read_body(arg: Option<&str>, what: &str) -> Result<String> {
    let body = match arg {
        Some("-") => read_stdin()?,
        Some(text) => text.to_string(),
        None if std::io::stdin().is_terminal() => {
            bail!("Pass the {what} as an argument, or pipe it on stdin")
        }
        None => read_stdin()?,
    };
    let body = body.trim_end_matches(['\n', '\r']).to_string();
    if body.trim().is_empty() {
        bail!("The {what} is empty");
    }
    Ok(body)
}

fn read_stdin() -> Result<String> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .context("Failed to read stdin")?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_confirms_without_asking() {
        assert!(confirm("Delete it?", true).unwrap());
    }

    #[test]
    fn body_argument_loses_trailing_newlines_only() {
        assert_eq!(
            read_body(Some("  hi\nthere\r\n\n"), "body").unwrap(),
            "  hi\nthere"
        );
        assert_eq!(read_body(Some("x \n"), "body").unwrap(), "x ");
    }

    #[test]
    fn blank_body_argument_is_rejected() {
        for text in ["", "\n", "  \n\t"] {
            let err = read_body(Some(text), "comment").unwrap_err();
            assert_eq!(err.to_string(), "The comment is empty");
        }
    }
}
