//! The unified diff a reconcile prints before it writes, and all `--dry-run` writes (LD-378).
//!
//! A longest-common-subsequence diff of lines, with three lines of context, in the `diff -u`
//! layout. The common head and tail are trimmed first, so the quadratic table covers only the
//! region that changed — a reconcile changes a few lines of a short file.

/// Lines of context around each change.
const CONTEXT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line<'a> {
    Same(&'a str),
    Gone(&'a str),
    New(&'a str),
}

/// The unified diff of `before` and `after`, labelled with `label`; empty when they are equal.
pub fn unified(label: &str, before: &str, after: &str) -> String {
    if before == after {
        return String::new();
    }
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let lines = script(&old, &new);
    let mut out = format!("--- {label}\n+++ {label} (reconciled)\n");
    // Hunks: runs of changes with their context, merged when their context touches.
    let changed: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !matches!(line, Line::Same(_)))
        .map(|(index, _)| index)
        .collect();
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for index in changed {
        let start = index.saturating_sub(CONTEXT);
        let end = (index + CONTEXT + 1).min(lines.len());
        match hunks.last_mut() {
            Some((_, last_end)) if start <= *last_end => *last_end = end,
            _ => hunks.push((start, end)),
        }
    }
    for (start, end) in hunks {
        let (mut old_at, mut new_at) = (1, 1);
        for line in &lines[..start] {
            match line {
                Line::Same(_) => {
                    old_at += 1;
                    new_at += 1;
                }
                Line::Gone(_) => old_at += 1,
                Line::New(_) => new_at += 1,
            }
        }
        let body = &lines[start..end];
        let old_count = body.iter().filter(|l| !matches!(l, Line::New(_))).count();
        let new_count = body.iter().filter(|l| !matches!(l, Line::Gone(_))).count();
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(old_at, old_count),
            range(new_at, new_count)
        ));
        for line in body {
            match line {
                Line::Same(text) => out.push_str(&format!(" {text}\n")),
                Line::Gone(text) => out.push_str(&format!("-{text}\n")),
                Line::New(text) => out.push_str(&format!("+{text}\n")),
            }
        }
    }
    out
}

/// `diff -u`'s spelling of a range: `start,count`, with an empty range starting one line early.
fn range(start: usize, count: usize) -> String {
    match count {
        0 => format!("{},0", start - 1),
        1 => start.to_string(),
        _ => format!("{start},{count}"),
    }
}

/// The edit script: the common head, the longest common subsequence of the middle, the tail.
fn script<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Line<'a>> {
    let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old[head..old.len() - tail];
    let b = &new[head..new.len() - tail];
    // lcs[i][j]: the length of the longest common subsequence of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out: Vec<Line<'a>> = old[..head].iter().map(|l| Line::Same(l)).collect();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(Line::Same(a[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(Line::Gone(a[i]));
            i += 1;
        } else {
            out.push(Line::New(b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| Line::Gone(l)));
    out.extend(b[j..].iter().map(|l| Line::New(l)));
    out.extend(old[old.len() - tail..].iter().map(|l| Line::Same(l)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_texts_have_no_diff() {
        assert_eq!(unified("host.toml", "a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn a_change_is_shown_with_three_lines_of_context_in_diff_u_layout() {
        let before = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let after = "1\n2\n3\n4\nfive\n6\n7\n8\n9\nten\n";
        assert_eq!(
            unified("host.toml", before, after),
            "--- host.toml\n+++ host.toml (reconciled)\n@@ -2,8 +2,9 @@\n 2\n 3\n 4\n-5\n+five\n 6\n 7\n 8\n 9\n+ten\n"
        );
    }

    #[test]
    fn distant_changes_are_separate_hunks() {
        let before: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let after: String = (1..=20)
            .filter(|n| *n != 19)
            .map(|n| {
                if n == 2 {
                    "two\n".to_string()
                } else {
                    format!("{n}\n")
                }
            })
            .collect();
        let diff = unified("m", &before, &after);
        assert_eq!(diff.matches("@@ -").count(), 2, "{diff}");
        assert!(diff.contains("@@ -1,5 +1,5 @@\n 1\n-2\n+two\n"), "{diff}");
        assert!(
            diff.contains("@@ -16,5 +16,4 @@\n 16\n 17\n 18\n-19\n 20\n"),
            "{diff}"
        );
    }
}
