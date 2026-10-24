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
        .map(|l| format!("{prefix}{l}").trim_end().to_string())
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

/// Align rows into columns separated by two spaces. The last column is left unpadded.
pub fn table(rows: &[Vec<String>], indent: &str) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..cols)
        .map(|c| {
            rows.iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    if i + 1 == row.len() {
                        cell.clone()
                    } else {
                        let pad = widths[i] - cell.chars().count();
                        format!("{cell}{}", " ".repeat(pad))
                    }
                })
                .collect();
            format!("{indent}{}", cells.join("  ").trim_end())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A run time such as `45s`, `3m12s`, or `1h05m`. Negative spans, from clock skew, read as `0s`.
pub fn duration(seconds: i64) -> String {
    let s = seconds.max(0);
    let (h, m, s) = (s / 3600, s % 3600 / 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_columns() {
        let rows = vec![
            vec!["#3".to_string(), "a".to_string(), "x".to_string()],
            vec!["#12".to_string(), "bbb".to_string(), "y".to_string()],
        ];
        assert_eq!(table(&rows, "  "), "  #3   a    x\n  #12  bbb  y");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(192), "3m12s");
        assert_eq!(duration(3900), "1h05m");
        assert_eq!(duration(-3), "0s");
    }

    #[test]
    fn first_lines_notes_dropped_count() {
        assert_eq!(first_lines("a\nb\nc", 2), "a\nb\n[+1 lines]");
        assert_eq!(first_lines("a\nb", 2), "a\nb");
    }
}
