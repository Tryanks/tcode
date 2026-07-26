//! Blocking work, run off the main thread on **gpui's** executor.
//!
//! The tempting alternative, `smol::unblock`, is a trap here. A gpui task is a
//! *local* task: gpui asserts that its runnable is only ever polled and dropped
//! by the thread that spawned it. Await a `smol::unblock` future inside one and
//! the wake — and, if the task is dropped while suspended, the drop — comes from
//! a thread in smol's global blocking pool. gpui's test scheduler catches that
//! ("local task dropped by a thread that didn't spawn it") and panics from a
//! pool thread, which aborts the whole process: it took down the Windows CI run
//! with a bare `STATUS_STACK_BUFFER_OVERRUN` and no test name attached.
//!
//! gpui's background executor is the one its scheduler owns — deterministic
//! under test, a real thread pool in production — so blocking work goes there.

use gpui::{BackgroundExecutor, Task};

/// Run `f` on gpui's background executor.
///
/// ```ignore
/// let status = blocking::unblock(cx.background_executor(), move || read_status(&cwd)).await;
/// ```
pub fn unblock<R, F>(executor: &BackgroundExecutor, f: F) -> Task<R>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    executor.spawn(async move { f() })
}

#[cfg(test)]
mod tests {
    /// Guard: no `smol::unblock` anywhere in the app. See the module docs — from
    /// inside a gpui task it aborts the process. Use [`super::unblock`].
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
            "smol::unblock inside a gpui task drops local tasks on a foreign \
             thread and aborts; use tcode_runtime::blocking::unblock: {offenders:#?}"
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
