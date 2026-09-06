//! Transitional GPUI adapter for the shared client-host seam.

use std::rc::Rc;

use gpui::{App, WindowInsets};

pub use tcode_client::ConnectionState;
pub use tcode_client::host::{
    ClientHost, ClientPreferences, DiscoveredHost, PairRequest, Transport, parse_discovered_hosts,
};
pub use tcode_client::pairing::{
    PairInvite, PairedHost, is_pairing_code, pair_url, parse_pair_url,
};

pub type MobilePreferences = ClientPreferences;
pub type BrowseDone = Box<dyn FnOnce(Vec<DiscoveredHost>, &mut App) + 'static>;
pub type PairDone = Box<dyn FnOnce(Result<PairedHost, String>, &mut App) + 'static>;
pub type ScanDone = Box<dyn FnOnce(Result<String, String>, &mut App) + 'static>;

/// Adapts GPUI-free client-host futures to the callbacks used by MobileRoot.
pub struct MobileHost {
    host: Rc<dyn ClientHost>,
    insets: Box<dyn Fn() -> WindowInsets>,
}

impl MobileHost {
    pub fn new(host: Rc<dyn ClientHost>) -> Self {
        Self {
            host,
            insets: Box::new(WindowInsets::default),
        }
    }

    pub fn with_insets(mut self, insets: impl Fn() -> WindowInsets + 'static) -> Self {
        self.insets = Box::new(insets);
        self
    }

    pub fn device_name(&self) -> String {
        self.host.device_name()
    }

    pub fn preferences(&self) -> MobilePreferences {
        self.host.load_preferences()
    }

    pub fn save_preferences(&self, preferences: &MobilePreferences) {
        self.host.save_preferences(preferences);
    }

    pub fn load_hosts(&self) -> Vec<PairedHost> {
        self.host.load_hosts()
    }

    pub fn save_hosts(&self, hosts: &[PairedHost]) {
        self.host.save_hosts(hosts);
    }

    pub fn last_host_id(&self) -> Option<String> {
        self.host.last_host_id()
    }

    pub fn set_last_host_id(&self, host_id: Option<&str>) {
        self.host.set_last_host_id(host_id);
    }

    pub fn fixed_pairing_endpoint(&self) -> Option<(String, u16)> {
        self.host.fixed_pairing_endpoint()
    }

    pub fn pair(&self, request: PairRequest, cx: &mut App, done: PairDone) {
        let host = self.host.clone();
        cx.spawn(async move |cx| {
            let result = host.pair(request).await;
            cx.update(|cx| done(result, cx));
        })
        .detach();
    }

    pub fn connect(&self, paired_host: &PairedHost) -> Transport {
        self.host.connect(paired_host)
    }

    pub fn browse_hosts(&self, done: BrowseDone, cx: &mut App) {
        let host = self.host.clone();
        cx.spawn(async move |cx| {
            let hosts = host.browse_hosts().await;
            cx.update(|cx| done(hosts, cx));
        })
        .detach();
    }

    pub fn certificate_changed(&self, host_id: &str) -> bool {
        self.host.certificate_changed(host_id)
    }

    pub fn supports_qr(&self) -> bool {
        self.host.supports_qr()
    }

    pub fn scan_qr(&self, done: ScanDone, cx: &mut App) {
        let host = self.host.clone();
        cx.spawn(async move |cx| {
            let result = host.scan_qr().await;
            cx.update(|cx| done(result, cx));
        })
        .detach();
    }

    pub fn insets(&self) -> WindowInsets {
        (self.insets)()
    }
}

pub type SharedHost = Rc<MobileHost>;

#[cfg(feature = "native")]
pub use tcode_remote::NativeClientHost as NativeHost;
