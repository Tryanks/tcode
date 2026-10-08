//! Read-only GitHub branch/summary/stack probe using the host's real gh login.
use tcode_services::{
    github::{Credentials, GitHubApi, pull_requests::PullRequests, repository},
    settings::SettingsStore,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::args()
        .nth(1)
        .ok_or("usage: github_pull_requests CHECKOUT")?;
    let root = std::env::temp_dir().join(format!("tcode-pr-live-{}", std::process::id()));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let credentials = Credentials::new(
            SettingsStore::new(root.clone()),
            std::env::vars().filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "GH_TOKEN" | "GITHUB_TOKEN" | "GH_ENTERPRISE_TOKEN" | "GITHUB_ENTERPRISE_TOKEN"
                )
            }),
        );
        let service = PullRequests::new(GitHubApi::host(credentials));
        let head = repository::branch_head(std::path::Path::new(&cwd))
            .ok_or("branch has no published GitHub head")?;
        let found = service
            .branch(&head, true)?
            .ok_or("no pull request for this branch")?;
        let summary = service.summary(&found.key, true)?;
        let stack = service.stack(&found.key, true)?;
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"branch":head.branch,"key":found.key,"snapshot":summary.snapshot,"summaryStackNumber":summary.stack_number,"stack":stack})
            )?
        );
        let unsupported = service.stack(
            &tcode_core::pull_request::PullRequestKey::new(
                "github.example.com",
                "tryanks/tcode",
                found.key.number,
            ),
            true,
        )?;
        println!(
            "Unsupported host stack: {}",
            serde_json::to_string(&unsupported)?
        );
        Ok(())
    })();
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    result
}
