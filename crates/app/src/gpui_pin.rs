//! Guards the pinned Zed/gpui revision.
//!
//! gpui comes from a git dependency with no `rev` in any manifest (the root
//! `Cargo.toml` explains why neither a manifest `rev` nor a `[patch]` can work
//! here), so Cargo.lock is the only thing holding the revision still. A bare
//! `cargo update` would move it to whatever Zed's default branch points at that
//! day, and the resulting lockfile diff is large enough that the one line that
//! matters is easy to miss in review.
//!
//! These tests make that move loud: bumping gpui means editing [`EXPECTED_ZED_REV`]
//! in the same commit as the lockfile, which is exactly the deliberate,
//! separately-reviewed bump the multiplatform plan asks for
//! (`docs/multiplatform-plan.md`, phase 0).

/// The Zed commit every `zed-industries/zed` package must resolve to.
///
/// zed v1.13.0 · gpui 0.2.2 · 2026-07-16.
const EXPECTED_ZED_REV: &str = "1a246efd7e1b83ab568ec5e3e6c1a43a42e1abba";

const ZED_REPO: &str = "git+https://github.com/zed-industries/zed";

/// Every distinct commit the lockfile resolves `zed-industries/zed` packages to.
///
/// A `source` line looks like
/// `source = "git+https://github.com/zed-industries/zed#<40-hex>"`, and gains a
/// `?rev=…` query when a manifest pins one.
fn locked_zed_revs(lockfile: &str) -> Vec<String> {
    let mut revs: Vec<String> = locked_zed_sources(lockfile)
        .iter()
        .filter_map(|source| Some(source.rsplit_once('#')?.1.to_owned()))
        .collect();
    revs.sort();
    revs.dedup();
    revs
}

/// Every distinct `zed-industries/zed` *source id* the lockfile mentions.
///
/// Distinct from [`locked_zed_revs`]: one commit can still appear under two
/// source ids, which is the duplicate-crate failure described on
/// `zed_packages_share_one_source_id`.
fn locked_zed_sources(lockfile: &str) -> Vec<String> {
    let mut sources: Vec<String> = lockfile
        .lines()
        .filter_map(|line| {
            let value = line.trim().strip_prefix("source = \"")?.strip_suffix('"')?;
            value.starts_with(ZED_REPO).then(|| value.to_owned())
        })
        .collect();
    sources.sort();
    sources.dedup();
    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lockfile() -> String {
        // crates/app -> crates -> workspace root
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("app manifest directory must be nested under workspace/crates");
        let path = workspace_root.join("Cargo.lock");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("workspace lockfile must be readable at {path:?}: {err}"))
    }

    #[test]
    fn locked_gpui_matches_the_recorded_revision() {
        let revs = locked_zed_revs(&lockfile());
        assert_eq!(
            revs,
            vec![EXPECTED_ZED_REV.to_owned()],
            "Cargo.lock resolves zed-industries/zed to a different commit than \
             EXPECTED_ZED_REV in crates/app/src/gpui_pin.rs. If this is a deliberate \
             gpui bump, update that constant in the same commit as the lockfile; \
             otherwise restore the lockfile (`git checkout Cargo.lock`)."
        );
    }

    /// A single commit is not enough — the packages must also share one *source
    /// id*. Adding `rev = ...` to our own gpui entries while `gpui-component`
    /// keeps depending on the unpinned URL yields `git+…#rev` and
    /// `git+…?rev=…#rev` side by side: two `gpui` crates at the same commit,
    /// whose types are nonetheless incompatible. That failure surfaces as a wall
    /// of unrelated-looking type errors, so name it here instead.
    #[test]
    fn zed_packages_share_one_source_id() {
        let sources = locked_zed_sources(&lockfile());
        assert_eq!(
            sources.len(),
            1,
            "zed-industries/zed resolved to {} distinct source ids, so the build \
             carries duplicate gpui crates whose types will not unify: {sources:#?}",
            sources.len()
        );
    }

    /// The lockfile shape both predicates parse. The first two lines are the
    /// duplicate-source state: one commit, two source ids.
    const SPLIT_SOURCE_LOCKFILE: &str = concat!(
        "source = \"git+https://github.com/zed-industries/zed#aaa\"\n",
        "source = \"git+https://github.com/zed-industries/zed?rev=aaa#aaa\"\n",
        "source = \"git+https://github.com/longbridge/gpui-component?rev=bbb#bbb\"\n",
        "source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    );

    #[test]
    fn rev_extraction_handles_both_pinned_and_unpinned_sources() {
        assert_eq!(
            locked_zed_revs(SPLIT_SOURCE_LOCKFILE),
            vec!["aaa".to_owned()]
        );
    }

    /// The duplicate-source guard must actually fire on the state it names —
    /// one commit reached through two source ids — rather than only ever seeing
    /// the healthy lockfile and passing vacuously.
    #[test]
    fn source_id_extraction_separates_pinned_from_unpinned() {
        let sources = locked_zed_sources(SPLIT_SOURCE_LOCKFILE);
        assert_eq!(
            sources.len(),
            2,
            "one commit under two source ids must count as two: {sources:#?}"
        );
        assert_eq!(locked_zed_revs(SPLIT_SOURCE_LOCKFILE).len(), 1);
    }
}
