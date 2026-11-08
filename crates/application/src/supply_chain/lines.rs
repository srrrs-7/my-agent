//! The fallback description: which lines a write adds and removes.

use std::collections::HashMap;

use agent_domain::model::dependency::{ChangedLines, DependencyChangeDescription};

/// Lines kept on each side. A new lockfile can be thousands of lines; the
/// human needs to see that it is large and what it starts with, not all of it.
const MAX_LINES: usize = 200;

/// Lines of `after` that `before` lacks and vice versa, compared as multisets
/// so that a moved line is not reported. Blank lines are left out.
pub(super) fn describe(reason: String, before: &str, after: &str) -> DependencyChangeDescription {
    let ChangedLines {
        added,
        removed,
        omitted,
    } = changed(significant(before), significant(after));
    DependencyChangeDescription::Lines {
        reason,
        added,
        removed,
        omitted,
    }
}

/// The lines in `after` that `before` lacks and vice versa, as multisets, the
/// first [`MAX_LINES`] of each side kept.
pub(super) fn changed<'a>(
    before: impl Iterator<Item = &'a str> + Clone,
    after: impl Iterator<Item = &'a str>,
) -> ChangedLines {
    let mut remaining: HashMap<&str, usize> = HashMap::new();
    for line in before.clone() {
        *remaining.entry(line).or_default() += 1;
    }

    let mut added = Vec::new();
    for line in after {
        match remaining.get_mut(line) {
            Some(count) if *count > 0 => *count -= 1,
            _ => added.push(line),
        }
    }

    let mut removed = Vec::new();
    for line in before {
        match remaining.get_mut(line) {
            Some(count) if *count > 0 => {
                *count -= 1;
                removed.push(line);
            }
            _ => {}
        }
    }

    let omitted = added.len().saturating_sub(MAX_LINES) + removed.len().saturating_sub(MAX_LINES);
    let keep = |lines: Vec<&str>| -> Vec<String> {
        lines
            .into_iter()
            .take(MAX_LINES)
            .map(str::to_string)
            .collect()
    };
    ChangedLines {
        added: keep(added),
        removed: keep(removed),
        omitted,
    }
}

pub(super) fn significant(text: &str) -> impl Iterator<Item = &str> + Clone {
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(before: &str, after: &str) -> (Vec<String>, Vec<String>, usize) {
        match describe(String::new(), before, after) {
            DependencyChangeDescription::Lines {
                added,
                removed,
                omitted,
                ..
            } => (added, removed, omitted),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reports_added_and_removed_lines_but_not_moved_ones() {
        let (added, removed, omitted) = lines("a\nb\nc\n", "c\na\nd\n");
        assert_eq!(added, ["d"]);
        assert_eq!(removed, ["b"]);
        assert_eq!(omitted, 0);
    }

    #[test]
    fn a_huge_change_keeps_the_first_lines_and_counts_the_rest() {
        let after: String = (0..MAX_LINES + 5).map(|n| format!("line {n}\n")).collect();
        let (added, _, omitted) = lines("", &after);
        assert_eq!(added.len(), MAX_LINES);
        assert_eq!(added[0], "line 0");
        assert_eq!(omitted, 5);
    }
}
