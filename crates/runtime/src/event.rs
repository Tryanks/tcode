//! Localization-free runtime events presented by the UI.
//!
//! The protocol owns every public event payload. Runtime keeps only this
//! internal envelope so [`crate::host::HostCx`] can distinguish an already
//! sequenced domain event from an unsequenced runtime notification.

pub use tcode_protocol::{
    GitActionRequest, RuntimeEffect, RuntimeError, RuntimeNotice,
    RuntimeNotification as RuntimeEvent, RuntimeOperationId, RuntimeToast,
};

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // The internal host queue stays directly typed.
pub enum HostEvent {
    Runtime(RuntimeEvent),
    Domain(tcode_protocol::EventEnvelope),
}
