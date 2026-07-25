//! GPUI-free infrastructure services shared by the tcode application layers.

// ACP installation is desktop-bound because it downloads, extracts, chmods, and executes tools.
#[cfg(feature = "desktop")]
pub mod acp_registry;
// Opening native applications is desktop-bound because it launches host processes.
#[cfg(feature = "desktop")]
pub mod desktop;
// Git services are desktop-bound because they shell out to git and read the working tree.
#[cfg(feature = "desktop")]
pub mod git;
// Transcript import is desktop-bound because it scans and reads provider files.
#[cfg(feature = "desktop")]
pub mod import;
// Process helpers are desktop-bound because browsers have no process model.
#[cfg(feature = "desktop")]
pub mod process;
pub mod provider_auth;
// Provider probing is desktop-bound because it resolves and executes provider binaries.
#[cfg(feature = "desktop")]
pub mod provider_probe;
// Relaunch markers are desktop-bound because they coordinate host filesystem state.
#[cfg(feature = "desktop")]
pub mod relaunch;
// Settings persistence is desktop-bound because the portable settings types live in tcode-core.
#[cfg(feature = "desktop")]
pub mod settings;
// Login-shell import is desktop-bound because it executes a host shell and mutates its environment.
#[cfg(feature = "desktop")]
pub mod shell_env;
// Session persistence is desktop-bound because it reads and writes the host filesystem.
#[cfg(feature = "desktop")]
pub mod store;
// User-file services are desktop-bound because they read, write, and remove host files.
#[cfg(feature = "desktop")]
pub mod user_files;
// Version checks are desktop-bound because install-source detection inspects local executables.
#[cfg(feature = "desktop")]
pub mod version_check;
// Workspace discovery is desktop-bound because it recursively scans the host filesystem.
#[cfg(feature = "desktop")]
pub mod workspace;
