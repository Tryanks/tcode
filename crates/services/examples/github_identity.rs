//! Live keyring/transport check. Prints only the verified login and id.
use tcode_services::{
    github::{Credentials, GitHubApi},
    settings::SettingsStore,
};
fn main() {
    let root = std::env::temp_dir().join(format!("tcode-github-live-{}", std::process::id()));
    let credentials = Credentials::new(
        SettingsStore::new(root),
        std::env::vars().filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "GH_TOKEN" | "GITHUB_TOKEN" | "GH_ENTERPRISE_TOKEN" | "GITHUB_ENTERPRISE_TOKEN"
            )
        }),
    );
    let api = GitHubApi::host(credentials);
    match api.verified_credential("github.com") {
        Ok((credential, identity)) => println!(
            "GET /user succeeded: login={}, id={}, source={:?}",
            identity.login,
            identity.id,
            credential.source()
        ),
        Err(error) => {
            eprintln!("GET /user failed: {error}");
            std::process::exit(1);
        }
    }
}
