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
