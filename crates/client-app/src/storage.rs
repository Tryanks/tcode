//! Where a shell persists the result of pairing, so a device pairs once rather
//! than every load.
//!
//! The concrete backend is injected — never chosen here — so this crate does
//! not learn what platform it runs on: the browser backs [`ConnectionStore`]
//! with `localStorage`, a phone will back it with a file, and neither has to
//! fork the client to do it.

use std::sync::{Arc, Mutex};

/// The durable outcome of pairing: which host, and the token that authenticates
/// to it.
///
/// The token is the secret. The URL is kept beside it only so a reload need not
/// re-type the address — a URL in an address bar was never the problem pairing
/// set out to solve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredConnection {
    pub url: String,
    pub token: String,
}

/// A place to keep one [`StoredConnection`] across loads.
pub trait ConnectionStore: Send + Sync {
    /// The saved connection, or `None` if this device has not paired.
    fn load(&self) -> Option<StoredConnection>;
    /// Persist `connection`, replacing any previous one.
    fn save(&self, connection: &StoredConnection);
    /// Forget the saved connection. Backs the sign-out action.
    fn clear(&self);
}

/// Drop a stored connection whose token cannot authenticate.
///
/// A blank token is no token: some backends return `Some("")` for a missing key
/// rather than `None`, and an empty token can only produce a doomed handshake —
/// so it must land the user on the connect screen, not in a reconnect loop.
pub fn usable_connection(stored: Option<StoredConnection>) -> Option<StoredConnection> {
    stored.filter(|connection| !connection.token.trim().is_empty())
}

/// A store that keeps its value in memory only.
///
/// For tests, and for any shell that deliberately does not persist. Cloneable
/// so a test can hold a handle to inspect what the client wrote.
#[derive(Clone, Default)]
pub struct MemoryStore {
    slot: Arc<Mutex<Option<StoredConnection>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a store as if the device had already paired.
    pub fn with(connection: StoredConnection) -> Self {
        Self {
            slot: Arc::new(Mutex::new(Some(connection))),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<StoredConnection>> {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A [`ConnectionStore`] backed by a single file.
///
/// The native shells' equivalent of the browser's `localStorage`: Android hands
/// it the app-private files dir, iOS the app container's Documents dir, both of
/// which the OS already isolates per app. Not built for wasm, which has no
/// filesystem — the browser uses `localStorage` instead.
///
/// Format is deliberately trivial: the URL on the first line, the token on the
/// second. Neither contains a newline (a URL cannot, and the host mints tokens
/// from a newline-free alphabet), so no escaping or serialization dependency is
/// needed to round-trip them.
#[cfg(not(target_family = "wasm"))]
pub struct FileStore {
    path: std::path::PathBuf,
}

#[cfg(not(target_family = "wasm"))]
impl FileStore {
    /// Keep the connection under `dir`, which the shell supplies (its app-private
    /// storage). The directory is created on save if it does not yet exist.
    pub fn new(dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: dir.into().join("sync-connection"),
        }
    }
}

#[cfg(not(target_family = "wasm"))]
impl ConnectionStore for FileStore {
    fn load(&self) -> Option<StoredConnection> {
        let contents = std::fs::read_to_string(&self.path).ok()?;
        // A file without the second line is corrupt, not a valid empty
        // connection, so it is treated as absent rather than half-read.
        let (url, token) = contents.split_once('\n')?;
        Some(StoredConnection {
            url: url.to_string(),
            token: token.trim_end_matches('\n').to_string(),
        })
    }

    fn save(&self, connection: &StoredConnection) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Best-effort, like the browser: failing to persist is not worth
        // interrupting a working session — the user simply pairs again next
        // launch.
        let _ = std::fs::write(
            &self.path,
            format!("{}\n{}", connection.url, connection.token),
        );
    }

    fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl ConnectionStore for MemoryStore {
    fn load(&self) -> Option<StoredConnection> {
        self.lock().clone()
    }

    fn save(&self, connection: &StoredConnection) {
        *self.lock() = Some(connection.clone());
    }

    fn clear(&self) {
        *self.lock() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(token: &str) -> StoredConnection {
        StoredConnection {
            url: "ws://host:1234/sync".into(),
            token: token.into(),
        }
    }

    #[test]
    fn a_saved_connection_survives_until_cleared() {
        let store = MemoryStore::new();
        assert_eq!(store.load(), None);

        store.save(&connection("durable"));
        assert_eq!(store.load(), Some(connection("durable")));

        store.clear();
        assert_eq!(store.load(), None, "sign-out must leave nothing behind");
    }

    #[test]
    fn an_empty_token_is_treated_as_no_connection() {
        // A backend that returns "" for a missing key must not send the client
        // into a handshake it cannot pass.
        assert_eq!(usable_connection(Some(connection(""))), None);
        assert_eq!(usable_connection(Some(connection("   "))), None);
        assert_eq!(
            usable_connection(Some(connection("real"))),
            Some(connection("real"))
        );
    }

    #[test]
    fn a_file_store_round_trips_and_clears() {
        // This test owns this directory name, so the process id is enough to
        // keep concurrent test binaries apart without a temp-file dependency.
        let dir =
            std::env::temp_dir().join(format!("tcode-filestore-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileStore::new(&dir);

        assert_eq!(store.load(), None, "nothing is stored before a save");
        store.save(&connection("durable"));
        assert_eq!(store.load(), Some(connection("durable")));

        store.clear();
        assert_eq!(store.load(), None, "clearing removes the file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
