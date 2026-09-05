//! UIScreen information supplied by the Swift host.

use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay, px, size};
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
pub(crate) struct IosDisplay;

impl PlatformDisplay for IosDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(1)
    }

    fn uuid(&self) -> anyhow::Result<Uuid> {
        Ok(Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            b"com.tryanks.tcode.gpui.primary-display",
        ))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        let metrics = super::ffi::host_metrics();
        Bounds::new(
            Default::default(),
            size(px(metrics.width), px(metrics.height)),
        )
    }
}
