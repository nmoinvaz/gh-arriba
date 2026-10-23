use anyhow::Result;
use serde::Serialize;

/// Print a value as pretty JSON on stdout.
pub fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Indent every line of `text` with `prefix`.
pub fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keep the first `max` lines of `text`, noting how many were dropped.
pub fn first_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    if lines.len() <= max {
        return lines.join("\n");
    }
    format!(
        "{}\n[+{} lines]",
        lines[..max].join("\n"),
        lines.len() - max
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_lines_notes_dropped_count() {
        assert_eq!(first_lines("a\nb\nc", 2), "a\nb\n[+1 lines]");
        assert_eq!(first_lines("a\nb", 2), "a\nb");
    }
}
