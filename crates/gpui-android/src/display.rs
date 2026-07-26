use std::{cell::Cell, fmt};

use anyhow::Result;
use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay};

use crate::AndroidDisplayMetrics;

pub(crate) struct AndroidDisplay {
    metrics: Cell<AndroidDisplayMetrics>,
}

impl AndroidDisplay {
    pub(crate) fn new(metrics: AndroidDisplayMetrics) -> Self {
        Self {
            metrics: Cell::new(metrics),
        }
    }

    pub(crate) fn update(&self, metrics: AndroidDisplayMetrics) {
        self.metrics.set(metrics);
    }
}

impl fmt::Debug for AndroidDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AndroidDisplay")
            .field("id", &DisplayId::new(1))
            .field("metrics", &self.metrics.get())
            .finish()
    }
}

impl PlatformDisplay for AndroidDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(1)
    }

    fn uuid(&self) -> Result<uuid::Uuid> {
        // Android's activity window is modeled as one stable logical display.
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
        // Android activities own a single full-surface window.
        self.bounds()
    }
}
