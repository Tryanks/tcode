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
        visit(std::path::Path::new("src"), &mut offenders);
        assert!(
            offenders.is_empty(),
            "smol::unblock inside a gpui task drops local tasks on a foreign \
             thread and aborts; use crate::blocking::unblock: {offenders:#?}"
        );
    }

    fn visit(dir: &std::path::Path, offenders: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, offenders);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            if path.file_name().is_some_and(|name| name == "blocking.rs") {
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
