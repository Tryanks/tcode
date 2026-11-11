use super::*;
use tcode_core::settings::TokenCheck;
use tcode_protocol::{CommandResponse, ProtocolError};

impl AppState {
    pub fn refresh_host_credentials(&mut self, cx: &HostCx) {
        self.host_generation += 1;
        let generation = self.host_generation;
        let forge = self.forge.clone();
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let status = host_cx.unblock(move || forge.credential_status()).await;
            host_cx.enqueue(move |state, _| {
                if generation == state.host_generation {
                    state.settings.source_control.status = status;
                }
            });
        });
    }

    /// A client looking at the hosts: besides what each reports, learn whose the saved or
    /// environment token of each is when that is not yet known, which asks the host.
    pub fn identify_host_credentials(&mut self, cx: &HostCx) {
        self.refresh_host_credentials(cx);
        let mut hosts: Vec<String> = self
            .settings
            .source_control
            .hosts
            .keys()
            .chain(self.settings.source_control.status.keys())
            .cloned()
            .chain(std::iter::once("github.com".to_owned()))
            .collect();
        hosts.sort();
        hosts.dedup();
        let host_cx = cx.clone();
        let tasks: Vec<_> = hosts
            .into_iter()
            .map(|host| {
                let forge = self.forge.clone();
                cx.unblock(move || forge.identify(&host))
            })
            .collect();
        HostCx::spawn_detached(cx, async move {
            for task in tasks {
                task.await;
            }
            host_cx.enqueue(|state, cx| state.refresh_host_credentials(cx));
        });
    }

    /// Threads that connect from now on read the configured hosts in their tool descriptions.
    pub(super) fn name_pull_request_hosts(&self) {
        if let Some(names) = &self.mcp.pull_request_hosts {
            names.set(tcode_core::pull_request::host_names(
                &self.pull_request_hosts(),
            ));
        }
    }

    pub fn set_host_token(
        &mut self,
        host: String,
        token: Option<String>,
        email: Option<String>,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        let saved = self.save_host_token(host, token, email, None, cx);
        cx.spawn_background(async move {
            saved.await?;
            Ok(CommandResponse::Unit)
        })
    }

    /// Saves the token only when the host takes it, recording whose it is.
    pub fn verify_host_token(
        &mut self,
        host: String,
        token: String,
        email: Option<String>,
        cx: &mut HostCx,
    ) -> HostTask<Result<CommandResponse, ProtocolError>> {
        let host = host.trim().to_ascii_lowercase();
        let token = token.trim().to_owned();
        let email = email
            .map(|email| email.trim().to_owned())
            .filter(|email| !email.is_empty());
        if token.is_empty() {
            return cx.spawn_background(async {
                Err(ProtocolError {
                    code: "invalid_token".into(),
                    message: "The token is empty.".into(),
                })
            });
        }
        let forge = self.forge.clone();
        let (asked_host, asked_token, asked_email) = (host.clone(), token.clone(), email.clone());
        let asked = cx
            .unblock(move || forge.check_token(&asked_host, &asked_token, asked_email.as_deref()));
        let host_cx = cx.clone();
        cx.spawn_background(async move {
            let check = asked.await;
            if let TokenCheck::Accepted { account } = &check {
                let login = account.as_ref().map(|account| account.login.clone());
                let saved = host_cx
                    .enqueue_and_wait(move |state, cx| {
                        state.save_host_token(host, Some(token), email, Some(login), cx)
                    })
                    .await
                    .map_err(|_| closed())?;
                saved.await?;
            }
            Ok(CommandResponse::TokenCheck(check))
        })
    }

    /// Writes (`Some`) or clears the host's token; `accepted` records that the host took it,
    /// signed in as the login when it said.
    fn save_host_token(
        &mut self,
        host: String,
        token: Option<String>,
        email: Option<String>,
        accepted: Option<Option<String>>,
        cx: &mut HostCx,
    ) -> HostTask<Result<(), ProtocolError>> {
        let host = host.trim().to_ascii_lowercase();
        let Some(kind) = self.settings.source_control.kind(&host) else {
            return cx.spawn_background(async {
                Err(ProtocolError {
                    code: "unknown_host_kind".into(),
                    message: "Choose what kind of host this is before giving it a token.".into(),
                })
            });
        };
        let (completion, receiver) = smol::channel::bounded(1);
        self.enqueue_store_write(
            StoreWrite::SetHostToken {
                kind,
                host: host.clone(),
                token,
                email,
                accepted,
                completion,
            },
            cx,
        );
        let host_cx = cx.clone();
        cx.spawn_background(async move {
            receiver
                .recv()
                .await
                .map_err(|_| closed())?
                .map_err(|error| ProtocolError {
                    code: "settings_write_failed".into(),
                    message: error,
                })?;
            host_cx.enqueue(move |state, cx| {
                state.forge.forget_credential(&host);
                state.refresh_host_credentials(cx);
            });
            Ok(())
        })
    }
}

fn closed() -> ProtocolError {
    ProtocolError {
        code: "store_closed".into(),
        message: "The settings writer stopped.".into(),
    }
}
