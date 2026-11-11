//! The client a headless test window runs on, set up as [`crate::run_shell`]
//! sets up the client it opens a window for.

use std::rc::Rc;

use gpui::App;
use tcode_client::{
    host::{ClientHost, FileFailure, Transport},
    pairing::PairedHost,
    speech::SpeechRecognizer,
};

/// A desktop client with nothing saved and no platform pickers, and the
/// dictation it is given.
struct Desktop(Option<Rc<dyn SpeechRecognizer>>);

impl ClientHost for Desktop {
    fn speech_recognizer(&self) -> Option<Rc<dyn SpeechRecognizer>> {
        self.0.clone()
    }
    fn device_name(&self) -> String {
        "Desktop".into()
    }
    fn device_id(&self) -> Result<String, FileFailure> {
        Ok("desktop".into())
    }
    fn device_platform(&self) -> Option<String> {
        None
    }
    fn load_hosts(&self) -> Result<Vec<PairedHost>, FileFailure> {
        Ok(Vec::new())
    }
    fn save_hosts(&self, _: &[PairedHost]) -> Result<(), FileFailure> {
        Ok(())
    }
    fn last_host_id(&self) -> Option<String> {
        None
    }
    fn set_last_host_id(&self, _: Option<&str>) {}
    fn connect(&self, host: &PairedHost) -> Transport {
        unreachable!("this client saved no machine, and {} is none", host.name)
    }
}

/// The theme and gpui-base, and this client's attachment: the process-wide
/// state every window of a client has before it opens.
pub(crate) fn init(cx: &mut App) {
    crate::theme::init(cx);
    attach(Rc::new(Desktop(None)), true, cx);
}

/// This desktop client dictates with `recognizer`, as a platform that
/// supplies its own does.
pub(crate) fn dictate_with(recognizer: Rc<dyn SpeechRecognizer>, cx: &mut App) {
    attach(Rc::new(Desktop(Some(recognizer))), true, cx);
}

/// This client is `host`, with or without a host in its own process, and
/// asks the window's shell for a host as the running app does.
pub(crate) fn attach(host: Rc<dyn ClientHost>, local: bool, cx: &mut App) {
    cx.set_global(crate::remote::ClientAttachment::new(
        host,
        local,
        crate::shell::request_host,
    ));
}
