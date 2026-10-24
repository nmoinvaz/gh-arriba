use regex::Regex;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::LazyLock;

/// A run of log lines around one or more errors.
#[derive(Debug, PartialEq, Serialize)]
pub struct Block {
    /// 1-based line of the first entry in the raw log, so `sed -n` can find it.
    pub line: usize,
    pub lines: Vec<String>,
}

/// The error excerpt of a job log.
#[derive(Debug, PartialEq, Serialize)]
pub struct Excerpt {
    pub blocks: Vec<Block>,
    /// Blocks past `max` that were left out.
    pub dropped: usize,
}

/// Lines of context shown before each error by default.
pub const CONTEXT: usize = 8;

/// Error blocks shown per job by default.
pub const MAX: usize = 5;

/// Where saved job logs go without `-o`, shared so later calls find logs saved earlier.
pub fn default_dir() -> PathBuf {
    std::env::temp_dir().join("gh-arriba-logs")
}

static TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x{feff}?\d{4}-\d{2}-\d{2}T[\d:.]+Z ?").unwrap());
static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07").unwrap());
/// Failure markers for logs without `##[error]` annotations.
static FALLBACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i:\b(?:fatal )?error(?:\[\w+\])?:|^\s*error\[)|\bFAILED\b|\bFAIL\b").unwrap()
});

/// One raw log line without its timestamp, escape codes, or `##[group]` prefix.
fn clean_line(line: &str) -> String {
    let line = TIMESTAMP.replace(line, "");
    let line = ANSI.replace_all(&line, "");
    line.strip_prefix("##[group]")
        .unwrap_or(&line)
        .trim_end()
        .to_string()
}

/// Errors in a job log with `context` lines before each, keeping the first `max` blocks.
/// `##[error]` annotations are used when present, common compiler and test markers otherwise.
pub fn excerpt(raw: &str, context: usize, max: usize) -> Excerpt {
    let lines: Vec<String> = raw.lines().map(clean_line).collect();
    let mut hits: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].starts_with("##[error]"))
        .collect();
    if hits.is_empty() {
        hits = (0..lines.len())
            .filter(|&i| FALLBACK.is_match(&lines[i]))
            .collect();
    }
    blocks(&lines, hits, context, max)
}

/// Lines of a job log matching `pattern`, with `context` lines before each, keeping the first
/// `max` blocks. Matching runs on cleaned lines, so timestamps and escape codes never match.
pub fn grep(raw: &str, pattern: &Regex, context: usize, max: usize) -> Excerpt {
    let lines: Vec<String> = raw.lines().map(clean_line).collect();
    let hits = (0..lines.len())
        .filter(|&i| pattern.is_match(&lines[i]))
        .collect();
    blocks(&lines, hits, context, max)
}

/// The excerpt a caller asked for: lines matching `pattern` when given, errors otherwise.
pub fn select(raw: &str, pattern: Option<&Regex>, context: usize, max: usize) -> Excerpt {
    match pattern {
        Some(re) => grep(raw, re, context, max),
        None => excerpt(raw, context, max),
    }
}

/// What to say when [`select`] found nothing.
pub fn nothing_found(pattern: Option<&str>) -> String {
    match pattern {
        Some(re) => format!("no lines match {re}"),
        None => "no error lines found".to_string(),
    }
}

/// Cut windows of `context` lines ending at each hit, merging windows that overlap or touch so
/// shared context prints once.
fn blocks(lines: &[String], hits: Vec<usize>, context: usize, max: usize) -> Excerpt {
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for i in hits {
        let start = i.saturating_sub(context);
        match windows.last_mut() {
            Some((_, end)) if start <= *end + 1 => *end = i,
            _ => windows.push((start, i)),
        }
    }

    let dropped = windows.len().saturating_sub(max);
    let blocks = windows
        .into_iter()
        .take(max)
        .map(|(start, end)| Block {
            line: start + 1,
            lines: lines[start..=end]
                .iter()
                .filter(|l| *l != "##[endgroup]")
                .cloned()
                .collect(),
        })
        .collect();
    Excerpt { blocks, dropped }
}

/// Render an excerpt with raw-log line numbers, indented by `prefix`.
pub fn render(excerpt: &Excerpt, prefix: &str) -> String {
    let mut out = String::new();
    for (i, block) in excerpt.blocks.iter().enumerate() {
        if i > 0 {
            out.push_str(&format!("{prefix}...\n"));
        }
        out.push_str(&format!("{prefix}line {}:\n", block.line));
        for l in &block.lines {
            out.push_str(format!("{prefix}  {l}").trim_end());
            out.push('\n');
        }
    }
    if excerpt.dropped > 0 {
        out.push_str(&format!("{prefix}[+{} more errors]\n", excerpt.dropped));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_timestamps_escapes_and_groups() {
        assert_eq!(
            clean_line("2026-10-20T21:30:01.1234567Z \x1b[31merror\x1b[0m: boom"),
            "error: boom"
        );
        assert_eq!(
            clean_line("\u{feff}2026-10-20T21:30:01.1Z ##[group]Run cargo build"),
            "Run cargo build"
        );
        assert_eq!(clean_line("plain"), "plain");
    }

    const LOG: &str = "\
2026-01-01T00:00:00.0Z ##[group]Run make
2026-01-01T00:00:01.0Z cc -c a.c
2026-01-01T00:00:02.0Z a.c:3: warning: unused
2026-01-01T00:00:03.0Z a.c:9: error: expected ';'
2026-01-01T00:00:04.0Z ##[endgroup]
2026-01-01T00:00:05.0Z ##[error]Process completed with exit code 2.
2026-01-01T00:00:06.0Z Post job cleanup.
2026-01-01T00:00:07.0Z ASAN_OPTIONS=abort_on_error=1
2026-01-01T00:00:08.0Z ##[error]Second failure";

    #[test]
    fn annotations_win_and_close_windows_merge() {
        let e = excerpt(LOG, 1, 10);
        assert_eq!(
            e.blocks,
            vec![
                Block {
                    line: 5,
                    lines: vec!["##[error]Process completed with exit code 2.".to_string()],
                },
                Block {
                    line: 8,
                    lines: vec![
                        "ASAN_OPTIONS=abort_on_error=1".to_string(),
                        "##[error]Second failure".to_string(),
                    ],
                },
            ]
        );
        assert_eq!(e.dropped, 0);

        // Windows that touch merge, so the compiler error shows with both annotations.
        let merged = excerpt(LOG, 2, 10);
        assert_eq!(merged.blocks.len(), 1);
        assert_eq!(merged.blocks[0].line, 4);
        assert_eq!(merged.blocks[0].lines[0], "a.c:9: error: expected ';'");
    }

    #[test]
    fn max_drops_later_blocks() {
        let e = excerpt(LOG, 0, 1);
        assert_eq!(e.blocks.len(), 1);
        assert_eq!(e.dropped, 1);
        assert!(render(&e, "  ").ends_with("  [+1 more errors]\n"));
    }

    #[test]
    fn fallback_markers_without_annotations() {
        let log =
            "ok\nerror[E0308]: mismatched types\nfine\ntest foo ... FAILED\nabort_on_error: 1";
        let e = excerpt(log, 0, 10);
        let starts: Vec<usize> = e.blocks.iter().map(|b| b.line).collect();
        assert_eq!(starts, vec![2, 4]);
        assert_eq!(
            render(&e, ""),
            "line 2:\n  error[E0308]: mismatched types\n...\nline 4:\n  test foo ... FAILED\n"
        );
    }

    #[test]
    fn grep_matches_cleaned_lines() {
        let pattern = Regex::new(r"(?i)abort_on_error|second").unwrap();
        assert_eq!(
            grep(LOG, &pattern, 0, 10).blocks,
            vec![Block {
                line: 8,
                lines: vec![
                    "ASAN_OPTIONS=abort_on_error=1".to_string(),
                    "##[error]Second failure".to_string(),
                ],
            }]
        );
        // The timestamp is stripped before matching.
        let stamp = Regex::new("2026-01-01").unwrap();
        assert!(grep(LOG, &stamp, 0, 10).blocks.is_empty());
    }
}
