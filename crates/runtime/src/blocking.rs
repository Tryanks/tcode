//! Blocking host work, centralized on smol's blocking pool.

use crate::host::{HostCx, HostTask};

/// Run blocking work through the runtime-owned host seam.
pub fn unblock_host<R, F>(cx: &HostCx, f: F) -> HostTask<R>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    cx.spawn_background(smol::unblock(f))
}

#[cfg(test)]
mod tests {
    /// Guard: direct blocking-pool use stays centralized here so host work
    /// cannot accidentally run inline on its single state-owner thread.
    #[test]
    fn no_smol_unblock_outside_this_module() {
        let mut offenders = Vec::new();
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let crates_dir = manifest_dir
            .parent()
            .expect("runtime manifest directory must be nested under workspace/crates")
            .to_path_buf();
        assert!(
            crates_dir.is_dir(),
            "workspace crates directory does not exist: {}",
            crates_dir.display()
        );

        let mut roots = Vec::new();
        let mut discovered = Vec::new();
        for entry in std::fs::read_dir(&crates_dir)
            .expect("workspace crates directory must be readable")
            .flatten()
        {
            let crate_dir = entry.path();
            let source_root = crate_dir.join("src");
            if !crate_dir.is_dir() || !source_root.is_dir() {
                continue;
            }
            discovered.push(entry.file_name().to_string_lossy().into_owned());
            roots.push(source_root);
        }
        roots.sort();
        discovered.sort();
        let expected = [
            "agent",
            "app",
            "computer-use-mcp",
            "core",
            "i18n",
            "orchestrate-mcp",
            "preview-mcp",
            "protocol",
            "runtime",
            "services",
            "term",
            "ui",
        ]
        .map(str::to_owned);
        assert_eq!(
            discovered,
            expected,
            "crate source roots under {} must exactly match the final workspace inventory",
            crates_dir.display()
        );
        let exempt = crates_dir.join("runtime/src/blocking.rs");
        for root in &roots {
            assert!(
                root.is_dir(),
                "crate source root does not exist or is not a directory: {}",
                root.display()
            );
            visit(root, &exempt, &mut offenders);
        }
        assert!(
            offenders.is_empty(),
            "call blocking host work through tcode_runtime::blocking::unblock_host: \
             {offenders:#?}"
        );
    }

    fn visit(dir: &std::path::Path, exempt: &std::path::Path, offenders: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, exempt, offenders);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            if path == exempt {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (idx, line) in source.lines().enumerate() {
                if line.contains("smol::unblock") && !line.trim_start().starts_with("//") {
                    offenders.push(format!("{}:{}", path.display(), idx + 1));
                }
            }
        }
    }
}
