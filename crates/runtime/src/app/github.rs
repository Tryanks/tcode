use super::*;
use tcode_protocol::{CommandResponse, ProtocolError};

impl AppState {
    pub fn refresh_github_credentials(&mut self, cx: &HostCx) {
        self.github_generation += 1;
        let generation = self.github_generation;
        let forge = self.forge.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let status = host_cx.unblock(move || forge.credential_status()).await;
            host_cx.enqueue(move |state, _| {
                if generation == state.github_generation {
                    state.settings.github.status = status;
                }
            });
        });
    }

    pub fn set_github_token(
        &mut self,
        host: String,
        token: Option<String>,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        let (completion, receiver) = smol::channel::bounded(1);
        self.enqueue_store_write(
            StoreWrite::SetGitHubToken {
                host: host.clone(),
                token,
                completion,
            },
            cx,
        );
        let host_cx = cx.clone();
        cx.spawn_background(async move {
            receiver
                .recv()
                .await
                .map_err(|_| ProtocolError {
                    code: "store_closed".into(),
                    message: "The settings writer stopped.".into(),
                })?
                .map_err(|error| ProtocolError {
                    code: "settings_write_failed".into(),
                    message: error,
                })?;
            host_cx.enqueue(move |state, cx| {
                state.forge.forget_credential(&host);
                state.refresh_github_credentials(cx);
            });
            Ok(CommandResponse::Unit)
        })
    }
}
