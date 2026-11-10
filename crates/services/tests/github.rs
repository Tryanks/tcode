#[test]
fn real_git_checkouts_resolve_the_gh_default_remote() {
    use tcode_services::github::repository;
    let root = std::env::temp_dir().join(format!("tcode-gh-remote-{}", uuid::Uuid::new_v4()));
    let cwd = root.join("checkout");
    std::fs::create_dir_all(&cwd).unwrap();
    let git = |args: &[&str]| {
        let output = tcode_services::process::command("git")
            .current_dir(&cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&[
        "remote",
        "add",
        "upstream",
        "https://github.com/sample/project.git",
    ]);
    git(&[
        "remote",
        "add",
        "origin",
        "git@github.com:ForkOwner/project.git",
    ]);
    git(&[
        "remote",
        "add",
        "gitlab",
        "https://gitlab.example.com/sample/project.git",
    ]);
    git(&["config", "remote.upstream.gh-resolved", "base"]);
    let subdirectory = cwd.join("nested");
    std::fs::create_dir(&subdirectory).unwrap();
    assert_eq!(
        repository::resolve(&tcode_services::forge::Checkout::new(&subdirectory), &[])
            .unwrap()
            .owner,
        "sample"
    );
    // gh honours the mark over its ranking of remote names, whatever else the checkout has.
    git(&["config", "--unset", "remote.upstream.gh-resolved"]);
    git(&["config", "remote.origin.gh-resolved", "base"]);
    assert_eq!(
        repository::resolve(&tcode_services::forge::Checkout::new(&subdirectory), &[])
            .unwrap()
            .owner,
        "forkowner"
    );
    let _ = std::fs::remove_dir_all(root);
}
