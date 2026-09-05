use anyhow::Result;
use gpui::{Bounds, DisplayId, Pixels, PlatformDisplay};
use std::{cell::Cell, fmt};
use uuid::Uuid;

pub(crate) struct AndroidDisplay {
    bounds: Cell<Bounds<Pixels>>,
}

impl AndroidDisplay {
    pub(crate) fn new(bounds: Bounds<Pixels>) -> Self {
        Self {
            bounds: Cell::new(bounds),
        }
    }

    pub(crate) fn set_bounds(&self, bounds: Bounds<Pixels>) {
        self.bounds.set(bounds);
    }
}

impl fmt::Debug for AndroidDisplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AndroidDisplay")
            .field("bounds", &self.bounds.get())
            .finish()
    }
}

impl PlatformDisplay for AndroidDisplay {
    fn id(&self) -> DisplayId {
        DisplayId::new(0)
    }

    fn uuid(&self) -> Result<Uuid> {
        Ok(Uuid::new_v5(&Uuid::NAMESPACE_OID, b"gpui-android-primary"))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn visible_bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }
}
