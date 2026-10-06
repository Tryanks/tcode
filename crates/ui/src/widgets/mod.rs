/// Shared callback shape for toggle-style widget events (`&bool` new state).
pub(crate) type ToggleHandler = std::rc::Rc<dyn Fn(&bool, &mut gpui::Window, &mut gpui::App)>;

pub mod button;
pub mod checkbox;
pub(crate) mod copy;
pub mod input;
pub mod kbd;
pub mod menu;
mod popover;
pub mod progress;
pub(crate) mod ring;
pub mod spinner;
pub mod switch;
pub mod tooltip;

pub use button::{Button, ButtonVariant, ButtonVariants};
pub use checkbox::Checkbox;
pub use input::{Input, Textarea};
pub use kbd::Kbd;
pub use popover::Popover;
pub use progress::Progress;
pub use spinner::Spinner;
pub use switch::Switch;
pub use tooltip::Tooltip;

/// Stops a click's propagation after letting the window text selection see
/// the release. gpui-base ends a selection gesture from a bubble-phase
/// MouseUp listener on the root, and a MouseDown anywhere begins one, so a
/// click handler that only stops propagation leaves the selection following
/// the pointer until the next release.
pub(crate) fn stop_click_propagation(window: &mut gpui::Window, cx: &mut gpui::App) {
    gpui_base::TextSelection::end(window, cx);
    cx.stop_propagation();
}
