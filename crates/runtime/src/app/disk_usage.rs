use tcode_protocol::DiskUsageState;
use tcode_services::disk_usage::{self, Account};

use super::providers::launch_env_for_profile;
use super::*;

impl AppState {
    /// A client asking for disk usage before anything was measured starts
    /// the first measurement; later ones get the last one until asked to
    /// measure again.
    pub(super) fn disk_usage_subscribed(&mut self, cx: &mut HostCx) {
        if self.disk_usage.last.is_none() {
            self.measure_disk_usage(cx);
        }
    }

    /// Measure the data dir and every native provider account's directories
    /// in the background, unless a measurement runs. Clients hear it start
    /// and end; the last result stays until the next one ends.
    pub(crate) fn measure_disk_usage(&mut self, cx: &mut HostCx) {
        if self.disk_usage.measuring {
            return;
        }
        self.disk_usage.measuring = true;
        self.emit_disk_usage(cx);
        let settings = self.settings.clone();
        let secrets = self.settings_store.clone();
        let data_dir = self.store.root().to_path_buf();
        let measured = cx.unblock(move || {
            let accounts: Vec<Account> = ProviderKind::NATIVE
                .into_iter()
                .flat_map(|kind| settings.profiles_for_kind(kind))
                .map(|profile| {
                    let launch_env = launch_env_for_profile(
                        &settings,
                        &profile.id,
                        secrets.profile_secrets(&profile.id),
                    );
                    Account {
                        provider: profile.kind,
                        dirs: disk_usage::state_dirs(profile.kind, &launch_env),
                        profile_id: profile.id,
                    }
                })
                .collect();
            disk_usage::measure(&data_dir, &accounts)
        });
        let host_cx = cx.clone();
        HostCx::spawn_detached(cx, async move {
            let usage = measured.await;
            host_cx.enqueue(move |state, cx| {
                state.disk_usage = DiskUsageState {
                    last: Some(usage),
                    measuring: false,
                };
                state.emit_disk_usage(cx);
            });
        });
    }

    fn emit_disk_usage(&self, cx: &mut HostCx) {
        cx.emit(HostEvent::Domain(EventEnvelope {
            request_id: None,
            topic: Topic::DiskUsage,
            event: ServerEvent::DiskUsageReplaced(self.disk_usage.clone()),
        }));
    }
}
