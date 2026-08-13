use gpui::Pixels;
use serde::{Deserialize, Serialize};

/// A size for tcode UI elements.
#[derive(Clone, Default, Copy, PartialEq, Eq, Debug, Deserialize, Serialize)]
pub enum Size {
    Size(Pixels),
    XSmall,
    Small,
    #[default]
    Medium,
    Large,
}

impl From<Pixels> for Size {
    fn from(size: Pixels) -> Self {
        Self::Size(size)
    }
}

/// A trait for setting the size of an element.
pub trait Sizable: Sized {
    fn with_size(self, size: impl Into<Size>) -> Self;

    #[inline(always)]
    fn xsmall(self) -> Self {
        self.with_size(Size::XSmall)
    }

    #[inline(always)]
    fn small(self) -> Self {
        self.with_size(Size::Small)
    }

    #[inline(always)]
    fn large(self) -> Self {
        self.with_size(Size::Large)
    }
}

// Phase 2 still uses upstream styled widgets. Keep their size vocabulary behind
// this boundary so call sites only depend on tcode's Sizable and Size.
impl<T: gpui_component::Sizable> Sizable for T {
    fn with_size(self, size: impl Into<Size>) -> Self {
        let size = match size.into() {
            Size::Size(px) => gpui_component::Size::Size(px),
            Size::XSmall => gpui_component::Size::XSmall,
            Size::Small => gpui_component::Size::Small,
            Size::Medium => gpui_component::Size::Medium,
            Size::Large => gpui_component::Size::Large,
        };
        gpui_component::Sizable::with_size(self, size)
    }
}
