use std::path::Path;
use tcode_core::pull_request::{StackRebaseFailure, StackRebaseStep};
use tcode_services::github::stack_rebase::{self, Identity, RebaseLayer};

/// A bare repository standing in for GitHub, holding a three-layer stack cut from `main`, with
/// `main` moved ahead since. Each layer changes its own file, so each has one commit of its own.
struct Remote {
    _root: tempdir::Dir,
    path: std::path::PathBuf,
    work: std::path::PathBuf,
}

mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = tcode_services::process::command("git")
        .current_dir(cwd)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

impl Remote {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "tcode-stack-remote-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("remote.git");
        let work = root.join("work");
        git(
            &root,
            &["init", "--quiet", "--bare", path.to_str().unwrap()],
        );
        // One `git fast-import` writes the history: `main`'s first commit, each layer on the one
        // below it, then `main` moved ahead.
        let data = |text: &str| format!("data {}\n{text}\n", text.len());
        let mut stream = String::new();
        let mut commit = |mark: usize,
                          branch: &str,
                          from: Option<usize>,
                          file: &str,
                          text: &str,
                          message: &str| {
            stream.push_str(&format!(
                "commit refs/heads/{branch}\nmark :{mark}\ncommitter Fixture <fixture@example.test> 1700000000 +0000\n{}",
                data(message)
            ));
            if let Some(from) = from {
                stream.push_str(&format!("from :{from}\n"));
            }
            stream.push_str(&format!("M 100644 inline {file}\n{}\n", data(text)));
        };
        commit(1, "main", None, "README.md", "start\n", "Initial");
        commit(
            2,
            "layer-1",
            Some(1),
            "layer-1.txt",
            "one\n",
            "layer-1: add",
        );
        commit(
            3,
            "layer-2",
            Some(2),
            "layer-2.txt",
            "one\n",
            "layer-2: add",
        );
        commit(
            4,
            "layer-3",
            Some(3),
            "layer-3.txt",
            "one\n",
            "layer-3: add",
        );
        commit(
            5,
            "main",
            Some(1),
            "README.md",
            "main moved ahead\n",
            "Docs",
        );
        let mut import = tcode_services::process::command("git")
            .current_dir(&path)
            .args(["fast-import", "--quiet"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut import.stdin.take().unwrap(), stream.as_bytes()).unwrap();
        assert!(import.wait().unwrap().success());
        git(
            &root,
            &[
                "clone",
                "--quiet",
                path.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        Self {
            _root: tempdir::Dir(root),
            path,
            work,
        }
    }

    fn commit(&self, file: &str, text: &str, message: &str) {
        std::fs::write(self.work.join(file), text).unwrap();
        git(&self.work, &["add", "-A"]);
        git(&self.work, &["commit", "--quiet", "-m", message]);
    }

    fn head(&self, branch: &str) -> String {
        git(&self.path, &["rev-parse", &format!("refs/heads/{branch}")])
    }

    fn layers(&self) -> Vec<RebaseLayer> {
        (1..=3)
            .map(|number| RebaseLayer {
                number,
                branch: format!("layer-{number}"),
                head: self.head(&format!("layer-{number}")),
            })
            .collect()
    }

    fn rebase(&self, layers: &[RebaseLayer]) -> Vec<StackRebaseStep> {
        stack_rebase::cascade(
            self.path.to_str().unwrap(),
            // An extra header keyed to a filesystem remote is never sent.
            "fixture-token",
            &Identity::new("Host", "host@example.test"),
            "main",
            layers,
            |_, _| {},
        )
    }
}

#[test]
fn each_layer_moves_only_its_own_commits_onto_the_rebased_layer_below() {
    let remote = Remote::new();
    let layers = remote.layers();
    let steps = remote.rebase(&layers);
    let main = remote.head("main");
    let heads: Vec<_> = (1..=3)
        .map(|n| remote.head(&format!("layer-{n}")))
        .collect();
    assert_eq!(
        steps,
        layers
            .iter()
            .zip(&heads)
            .map(|(layer, head)| StackRebaseStep::Pushed {
                from: layer.head.clone(),
                to: head.clone(),
            })
            .collect::<Vec<_>>()
    );
    let parents = [&main, &heads[0], &heads[1]];
    for (index, head) in heads.iter().enumerate() {
        assert_eq!(
            &git(&remote.path, &["rev-parse", &format!("{head}~1")]),
            parents[index]
        );
        assert_eq!(
            git(
                &remote.path,
                &["log", "--format=%s", &format!("{}..{head}", parents[index])]
            ),
            format!("layer-{}: add", index + 1)
        );
        assert_eq!(
            git(&remote.path, &["log", "-1", "--format=%cn <%ce>", head]),
            "Host <host@example.test>"
        );
    }
    // Rebased already, the stack has nothing to move.
    let again = remote.rebase(&remote.layers());
    assert_eq!(again, vec![StackRebaseStep::AlreadyCurrent; 3]);
}

#[test]
fn the_lease_keeps_a_branch_pushed_to_since_review_and_the_layer_below_stays_pushed() {
    let remote = Remote::new();
    let reviewed = remote.layers();
    git(&remote.work, &["switch", "--quiet", "layer-2"]);
    remote.commit("layer-2.txt", "two\n", "layer-2: pushed meanwhile");
    git(&remote.work, &["push", "--quiet", "origin", "layer-2"]);
    let concurrent = remote.head("layer-2");
    let steps = remote.rebase(&reviewed);
    assert!(matches!(steps[0], StackRebaseStep::Pushed { .. }));
    assert_eq!(
        steps[1..],
        [
            StackRebaseStep::Failed {
                reason: StackRebaseFailure::LeaseRefused
            },
            StackRebaseStep::NotStarted
        ]
    );
    assert_eq!(remote.head("layer-2"), concurrent);
    assert_eq!(remote.head("layer-3"), reviewed[2].head);
    assert_eq!(
        git(
            &remote.path,
            &["rev-parse", &format!("{}~1", remote.head("layer-1"))]
        ),
        remote.head("main")
    );
}

#[test]
fn a_conflict_stops_at_the_failing_layer_untouched_with_the_lower_layers_pushed() {
    let remote = Remote::new();
    git(&remote.work, &["switch", "--quiet", "main"]);
    remote.commit("layer-2.txt", "main's own\n", "main writes layer 2's file");
    git(&remote.work, &["push", "--quiet", "origin", "main"]);
    let reviewed = remote.layers();
    let steps = remote.rebase(&reviewed);
    assert!(matches!(steps[0], StackRebaseStep::Pushed { .. }));
    assert_eq!(
        steps[1..],
        [
            StackRebaseStep::Failed {
                reason: StackRebaseFailure::Conflict
            },
            StackRebaseStep::NotStarted
        ]
    );
    assert_ne!(remote.head("layer-1"), reviewed[0].head);
    assert_eq!(remote.head("layer-2"), reviewed[1].head);
    assert_eq!(remote.head("layer-3"), reviewed[2].head);
}
