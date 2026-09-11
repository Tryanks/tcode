use crate::text_input::TextInputState;
use android_activity::AndroidApp;
use gpui::TextInputConfiguration;
use jni::{
    JavaVM,
    objects::{JObject, JString, JValue},
};
use parking_lot::Mutex;
use std::collections::VecDeque;

#[derive(Debug)]
pub(crate) enum HostEvent {
    CommitText(String),
    SetComposingText(String),
    FinishComposing,
    DeleteBackward,
    InputState {
        revision: u64,
        serial: u64,
        state: TextInputState,
    },
    Key {
        key_code: i32,
        down: bool,
        unicode_code_point: i32,
        meta_state: i32,
    },
    Insets {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        ime_bottom: i32,
    },
    Back,
}

pub(super) static APP: Mutex<Option<AndroidApp>> = Mutex::new(None);
static EVENTS: Mutex<VecDeque<HostEvent>> = Mutex::new(VecDeque::new());

pub(crate) fn initialize(app: &AndroidApp) {
    *APP.lock() = Some(app.clone());
}

fn enqueue(event: HostEvent) {
    EVENTS.lock().push_back(event);
    if let Some(app) = APP.lock().as_ref() {
        app.create_waker().wake();
    }
}

pub(crate) fn drain() -> Vec<HostEvent> {
    EVENTS.lock().drain(..).collect()
}

enum OwnedArgument {
    Bool(bool),
    Int(i32),
    Long(u64),
    Text(Option<String>),
}

impl OwnedArgument {
    fn as_jvalue<'a>(&self, text: &'a JObject<'a>) -> JValue<'a, 'a> {
        match self {
            Self::Bool(value) => JValue::Bool(u8::from(*value)),
            Self::Int(value) => JValue::Int(*value),
            Self::Long(value) => JValue::Long(*value as i64),
            Self::Text(_) => JValue::Object(text),
        }
    }
}

fn with_activity(method: &'static str, signature: &'static str, args: Vec<OwnedArgument>) {
    let Some(app) = APP.lock().clone() else {
        return;
    };
    let callback_app = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        // SAFETY: Android owns this VM for the duration of the process.
        let Ok(vm) = (unsafe { JavaVM::from_raw(callback_app.vm_as_ptr().cast()) }) else {
            log::error!("unable to access Android JavaVM");
            return;
        };
        let Ok(mut env) = vm.get_env() else {
            log::error!("Android UI thread is not attached to JavaVM");
            return;
        };
        // SAFETY: `activity_as_ptr` is the live NativeActivity instance.
        let activity = unsafe { JObject::from_raw(callback_app.activity_as_ptr().cast()) };
        let strings = args
            .iter()
            .map(|arg| match arg {
                OwnedArgument::Text(Some(text)) => env.new_string(text).map(JObject::from),
                _ => Ok(JObject::null()),
            })
            .collect::<jni::errors::Result<Vec<_>>>();
        let strings = match strings {
            Ok(strings) => strings,
            Err(error) => {
                log::error!("JNI {method} string allocation failed: {error}");
                let _ = env.exception_clear();
                return;
            }
        };
        let args = args
            .iter()
            .zip(&strings)
            .map(|(arg, text)| arg.as_jvalue(text))
            .collect::<Vec<_>>();
        if let Err(error) = env.call_method(&activity, method, signature, &args) {
            log::error!("JNI {method} failed: {error}");
            let _ = env.exception_clear();
        }
    }));
}

pub(crate) fn show_keyboard() {
    with_activity("gpuiShowKeyboard", "()V", Vec::new());
}

pub(crate) fn hide_keyboard() {
    with_activity("gpuiHideKeyboard", "()V", Vec::new());
}

pub(crate) fn configure_input(configuration: TextInputConfiguration) {
    // GPUI has no separate multiline flag; Enter explicitly requests a line break.
    with_activity(
        "gpuiConfigureInput",
        "(ZIZIZ)V",
        vec![
            OwnedArgument::Bool(configuration.autocorrect),
            OwnedArgument::Int(configuration.autocapitalize as i32),
            OwnedArgument::Bool(configuration.suggestions),
            OwnedArgument::Int(configuration.input_action as i32),
            OwnedArgument::Bool(configuration.input_action == gpui::TextInputAction::Enter),
        ],
    );
}

pub(crate) fn finish_activity() {
    with_activity("gpuiFinish", "()V", Vec::new());
}

pub(crate) fn read_clipboard() -> Option<String> {
    let app = APP.lock().clone()?;
    // SAFETY: Android owns this VM for the duration of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    // SAFETY: `activity_as_ptr` is the live NativeActivity instance.
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let object = match env.call_method(&activity, "gpuiReadClipboard", "()Ljava/lang/String;", &[])
    {
        Ok(value) => value.l().ok()?,
        Err(error) => {
            log::error!("JNI gpuiReadClipboard failed: {error}");
            let _ = env.exception_clear();
            return None;
        }
    };
    if object.is_null() {
        return None;
    }
    env.get_string(&JString::from(object)).ok().map(Into::into)
}

pub(crate) fn write_clipboard(text: String) {
    let Some(app) = APP.lock().clone() else {
        return;
    };
    let callback_app = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        // SAFETY: Android owns this VM for the duration of the process.
        let Ok(vm) = (unsafe { JavaVM::from_raw(callback_app.vm_as_ptr().cast()) }) else {
            log::error!("unable to access Android JavaVM");
            return;
        };
        let Ok(mut env) = vm.get_env() else {
            log::error!("Android UI thread is not attached to JavaVM");
            return;
        };
        // SAFETY: `activity_as_ptr` is the live NativeActivity instance.
        let activity = unsafe { JObject::from_raw(callback_app.activity_as_ptr().cast()) };
        let Ok(text) = env.new_string(text) else {
            log::error!("unable to allocate Android clipboard string");
            return;
        };
        if let Err(error) = env.call_method(
            &activity,
            "gpuiWriteClipboard",
            "(Ljava/lang/String;)V",
            &[JValue::Object(text.as_ref())],
        ) {
            log::error!("JNI gpuiWriteClipboard failed: {error}");
            let _ = env.exception_clear();
        }
    }));
}

pub fn commit_text(text: String) {
    enqueue(HostEvent::CommitText(text));
}

pub fn set_composing_text(text: String) {
    enqueue(HostEvent::SetComposingText(text));
}

pub fn finish_composing_text() {
    enqueue(HostEvent::FinishComposing);
}

pub fn delete_backward() {
    enqueue(HostEvent::DeleteBackward);
}

pub fn input_state(revision: u64, serial: u64, state: TextInputState) {
    enqueue(HostEvent::InputState {
        revision,
        serial,
        state,
    });
}

pub(crate) fn sync_input(revision: u64, serial: u64, state: Option<TextInputState>) {
    let selection = state.as_ref().map_or(0..0, |state| state.selection.clone());
    let marked = state.as_ref().and_then(|state| state.marked.clone());
    with_activity(
        "gpuiSyncInput",
        "(JJLjava/lang/String;IIII)V",
        vec![
            OwnedArgument::Long(revision),
            OwnedArgument::Long(serial),
            OwnedArgument::Text(state.map(|state| state.text)),
            OwnedArgument::Int(selection.start as i32),
            OwnedArgument::Int(selection.end as i32),
            OwnedArgument::Int(marked.as_ref().map_or(-1, |range| range.start as i32)),
            OwnedArgument::Int(marked.as_ref().map_or(-1, |range| range.end as i32)),
        ],
    );
}

pub fn key_event(key_code: i32, down: bool, unicode_code_point: i32, meta_state: i32) {
    enqueue(HostEvent::Key {
        key_code,
        down,
        unicode_code_point,
        meta_state,
    });
}

pub fn on_insets(left: i32, top: i32, right: i32, bottom: i32, ime_bottom: i32) {
    enqueue(HostEvent::Insets {
        left,
        top,
        right,
        bottom,
        ime_bottom,
    });
}

pub fn on_back() {
    enqueue(HostEvent::Back);
}

/// Rasterize on the calling render thread; a software Canvas needs no UI-thread hop.
pub(crate) fn rasterize_emoji(glyph: u32, size: f32) -> anyhow::Result<Option<Vec<i32>>> {
    let app = APP
        .lock()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Android host not initialized"))?;
    // SAFETY: Android owns the VM and live activity for the lifetime of this app handle.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }?;
    let mut env = vm.attach_current_thread()?;
    let result = env.with_local_frame(4, |env| -> anyhow::Result<Option<Vec<i32>>> {
        // SAFETY: the app handle keeps this NativeActivity alive.
        let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
        let object = env
            .call_method(
                &activity,
                "gpuiRasterizeEmoji",
                "(IF)[I",
                &[JValue::Int(glyph.try_into()?), JValue::Float(size)],
            )?
            .l()?;
        if object.is_null() {
            return Ok(None);
        }
        let array = jni::objects::JIntArray::from(object);
        let mut pixels = vec![0; env.get_array_length(&array)? as usize];
        env.get_int_array_region(&array, 0, &mut pixels)?;
        Ok(Some(pixels))
    });
    if result.is_err() {
        let _ = env.exception_clear();
    }
    result
}
