//! Workspace-entry filtering for the `@`-mention popover.

use tcode_runtime::ui_facade::PathEntry;

/// A ranked filter over workspace entries (case-insensitive), capped at `limit`.
/// Basename matches rank above path-only matches; a basename prefix match ranks
/// first. Empty query returns the first `limit` entries.
pub fn filter_entries<'a>(
    entries: &'a [PathEntry],
    query: &str,
    limit: usize,
) -> Vec<&'a PathEntry> {
    if query.is_empty() {
        return entries.iter().take(limit).collect();
    }
    let q = query.to_lowercase();
    let mut scored: Vec<(u8, usize, &PathEntry)> = Vec::new();
    for entry in entries {
        let base = entry.basename.to_lowercase();
        let path = entry.rel_path.to_lowercase();
        let rank = if base.starts_with(&q) {
            0
        } else if base.contains(&q) {
            1
        } else if path.contains(&q) {
            2
        } else {
            continue;
        };
        scored.push((rank, entry.rel_path.len(), entry));
    }
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.rel_path.cmp(&b.2.rel_path))
    });
    scored.into_iter().take(limit).map(|(_, _, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_ranks_basename_prefix_first() {
        let entries = vec![
            PathEntry {
                rel_path: "src/composer.rs".into(),
                basename: "composer.rs".into(),
                parent: "src".into(),
                is_dir: false,
            },
            PathEntry {
                rel_path: "docs/decompose.md".into(),
                basename: "decompose.md".into(),
                parent: "docs".into(),
                is_dir: false,
            },
            PathEntry {
                rel_path: "src/ui/composer_trigger.rs".into(),
                basename: "composer_trigger.rs".into(),
                parent: "src/ui".into(),
                is_dir: false,
            },
        ];
        let out = filter_entries(&entries, "compo", 10);
        // `composer.rs` (basename prefix) ranks above `decompose.md` (basename
        // contains) which ranks above nothing here.
        assert_eq!(out[0].rel_path, "src/composer.rs");
        assert!(
            out.iter()
                .any(|e| e.rel_path == "src/ui/composer_trigger.rs")
        );
        assert!(out.iter().any(|e| e.rel_path == "docs/decompose.md"));
    }

    #[test]
    fn empty_query_returns_capped_prefix() {
        let entries: Vec<PathEntry> = (0..10)
            .map(|i| PathEntry {
                rel_path: format!("f{i}.txt"),
                basename: format!("f{i}.txt"),
                parent: String::new(),
                is_dir: false,
            })
            .collect();
        assert_eq!(filter_entries(&entries, "", 3).len(), 3);
    }
}
