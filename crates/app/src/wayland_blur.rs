//! Whether the Wayland compositor blurs behind a `Blurred` window.
//!
//! gpui-fast-linux 0.1.13 makes a `Blurred` window transparent and asks for
//! blur without saying whether the compositor applies it: through
//! ext-background-effect whenever that global is advertised, whatever blur
//! capability it reports, and through org_kde_kwin_blur only when it is not.
//! This asks the compositor the same way. Remove it once GPUI reports whether
//! blur took effect.

use std::sync::OnceLock;

use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry,
};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
    self, Capability, ExtBackgroundEffectManagerV1,
};

const KDE_BLUR_MANAGER: &str = "org_kde_kwin_blur_manager";

/// Decided once: the theme and every window must agree on it.
pub(crate) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(probe)
}

fn probe() -> bool {
    let Ok(connection) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, mut queue)) = registry_queue_init::<Probe>(&connection) else {
        return false;
    };
    match globals.bind::<ExtBackgroundEffectManagerV1, _, _>(&queue.handle(), 1..=1, ()) {
        Ok(manager) => {
            let mut probe = Probe { blur: false };
            let blur = queue.roundtrip(&mut probe).is_ok() && probe.blur;
            manager.destroy();
            blur
        }
        Err(_) => globals.contents().with_list(|list| {
            list.iter()
                .any(|global| global.interface == KDE_BLUR_MANAGER)
        }),
    }
}

struct Probe {
    blur: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        event: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_background_effect_manager_v1::Event::Capabilities {
            flags: WEnum::Value(flags),
        } = event
        {
            probe.blur = flags.contains(Capability::Blur);
        }
    }
}
