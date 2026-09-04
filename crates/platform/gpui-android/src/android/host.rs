use android_activity::AndroidApp;
use gpui::TextInputConfiguration;
use gpui_wgpu::ColorGlyphRaster;
use jni::{
    JavaVM,
    objects::{JByteArray, JObject, JString, JValue},
};
use parking_lot::Mutex;
use std::collections::VecDeque;

#[derive(Debug)]
pub(crate) enum HostEvent {
    CommitText(String),
    SetComposingText(String),
    FinishComposing,
    DeleteBackward,
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

static APP: Mutex<Option<AndroidApp>> = Mutex::new(None);
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
}

impl OwnedArgument {
    fn as_jvalue(&self) -> JValue<'static, 'static> {
        match self {
            Self::Bool(value) => JValue::Bool(u8::from(*value)),
            Self::Int(value) => JValue::Int(*value),
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
        let args = args
            .iter()
            .map(OwnedArgument::as_jvalue)
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
    with_activity(
        "gpuiConfigureInput",
        "(ZIZI)V",
        vec![
            OwnedArgument::Bool(configuration.autocorrect),
            OwnedArgument::Int(configuration.autocapitalize as i32),
            OwnedArgument::Bool(configuration.suggestions),
            OwnedArgument::Int(configuration.input_action as i32),
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

pub(crate) fn rasterize_color_emoji(glyph_id: u32, pixel_size: f32) -> Option<ColorGlyphRaster> {
    let app = APP.lock().clone()?;
    // SAFETY: Android owns this VM for the duration of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    // SAFETY: `activity_as_ptr` is the live NativeActivity instance.
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let glyph_id = i32::try_from(glyph_id).ok()?;
    let object = match env.call_method(
        &activity,
        "gpuiRasterizeColorEmoji",
        "(IF)[B",
        &[JValue::Int(glyph_id), JValue::Float(pixel_size)],
    ) {
        Ok(value) => value.l().ok()?,
        Err(error) => {
            log::error!("JNI gpuiRasterizeColorEmoji failed: {error}");
            let _ = env.exception_clear();
            return None;
        }
    };
    if object.is_null() {
        return None;
    }

    let bytes = env.convert_byte_array(JByteArray::from(object)).ok()?;
    let header: [u8; 16] = bytes.get(..16)?.try_into().ok()?;
    let left = i32::from_le_bytes(header[0..4].try_into().ok()?);
    let top = i32::from_le_bytes(header[4..8].try_into().ok()?);
    let width = u32::try_from(i32::from_le_bytes(header[8..12].try_into().ok()?)).ok()?;
    let height = u32::try_from(i32::from_le_bytes(header[12..16].try_into().ok()?)).ok()?;
    let expected = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)?;
    let data = bytes.get(16..)?;
    if width == 0 || height == 0 || data.len() != expected {
        return None;
    }

    Some(ColorGlyphRaster {
        left,
        top,
        width,
        height,
        data: data.to_vec(),
    })
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

pub(super) fn commit_text(text: String) {
    enqueue(HostEvent::CommitText(text));
}

pub(super) fn set_composing_text(text: String) {
    enqueue(HostEvent::SetComposingText(text));
}

pub(super) fn finish_composing_text() {
    enqueue(HostEvent::FinishComposing);
}

pub(super) fn delete_backward() {
    enqueue(HostEvent::DeleteBackward);
}

pub(super) fn key_event(key_code: i32, down: bool, unicode_code_point: i32, meta_state: i32) {
    enqueue(HostEvent::Key {
        key_code,
        down,
        unicode_code_point,
        meta_state,
    });
}

pub(super) fn on_insets(left: i32, top: i32, right: i32, bottom: i32, ime_bottom: i32) {
    enqueue(HostEvent::Insets {
        left,
        top,
        right,
        bottom,
        ime_bottom,
    });
}

pub(super) fn on_back() {
    enqueue(HostEvent::Back);
}
