use std::{cell::Cell, fmt};

use anyhow::Result;
use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay};

use crate::IosDisplayMetrics;

pub(crate) struct IosDisplay {
    metrics: Cell<IosDisplayMetrics>,
}

impl IosDisplay {
    pub(crate) fn new(metrics: IosDisplayMetrics) -> Self {
        Self {
            metrics: Cell::new(metrics),
        }
    }

    pub(crate) fn update(&self, metrics: IosDisplayMetrics) {
        self.metrics.set(metrics);
    }
}

impl fmt::Debug for IosDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IosDisplay")
            .field("id", &DisplayId::new(1))
            .field("metrics", &self.metrics.get())
            .finish()
    }
}

impl PlatformDisplay for IosDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(1)
    }

    fn uuid(&self) -> Result<uuid::Uuid> {
        // iOS exposes one scene per app, modeled as a single stable logical display.
        Ok(uuid::Uuid::from_u128(
            0x69d9_8b2a_37c3_4dca_a37d_726f_6964_0001,
        ))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.metrics.get().logical_bounds()
    }

    fn visible_bounds(&self) -> Bounds<Pixels> {
        // System bars and cutouts are overlays represented by WindowInsets.
        self.bounds()
    }

    fn default_bounds(&self) -> Bounds<Pixels> {
        // An iOS scene owns a single full-screen window.
        self.bounds()
    }
}
