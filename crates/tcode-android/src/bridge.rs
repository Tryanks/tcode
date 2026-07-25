//! `AndroidHost` implemented over JNI.
//!
//! The one structural rule, from `crates/gpui-android/README.md`: hold a
//! `JavaVM` and global references, never a thread-local `JNIEnv`. GPUI calls
//! `wake_main_thread` from Rust worker threads, and a `JNIEnv` is only valid on
//! the thread that obtained it — so every method here attaches to the VM itself
//! rather than capturing an environment.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result, anyhow};
use gpui::{Bounds, Pixels, TextInputStateChange, ThermalState, WindowAppearance};
use gpui_android::AndroidHost;
use jni::objects::{GlobalRef, JObject, JValue};
use jni::{JNIEnv, JavaVM};

/// Java-side methods this bridge calls. Kept in one place so the Kotlin/Java
/// side has a single list to satisfy, and so a rename shows up as one diff
/// rather than scattered string literals.
mod method {
    pub const WAKE: &str = "wakeMainThread";
    pub const REQUEST_FRAME: &str = "requestFrame";
    pub const FINISH: &str = "finishActivity";
    pub const OPEN_URI: &str = "openUri";
    pub const READ_CLIPBOARD: &str = "readClipboardText";
    pub const WRITE_CLIPBOARD: &str = "writeClipboardText";
    pub const SHOW_KEYBOARD: &str = "showSoftKeyboard";
    pub const HIDE_KEYBOARD: &str = "hideSoftKeyboard";
    pub const SET_BACK_ENABLED: &str = "setBackEnabled";
    pub const IS_DARK_MODE: &str = "isDarkMode";
}

pub struct JniHost {
    vm: JavaVM,
    /// The `TcodeActivity` (or any object implementing the methods above).
    activity: GlobalRef,
    /// True while a Choreographer callback is already scheduled.
    ///
    /// The contract requires `request_frame` to coalesce: GPUI may ask for a
    /// frame many times within one vsync interval, and posting a callback per
    /// request would queue redundant frames behind each other. `AndroidEventSink`
    /// rearms after it fires, so this only has to suppress duplicates in
    /// between.
    frame_pending: AtomicBool,
}

impl JniHost {
    pub fn new(env: &JNIEnv<'_>, activity: &JObject<'_>) -> Result<Self> {
        Ok(Self {
            vm: env.get_java_vm().context("retaining the JavaVM")?,
            activity: env
                .new_global_ref(activity)
                .context("retaining a global ref to the activity")?,
            frame_pending: AtomicBool::new(false),
        })
    }

    /// Attach to the VM and run `body` with a usable environment.
    ///
    /// `attach_current_thread` is the reason the VM is stored rather than an
    /// env: it works from any thread, including the Rust worker threads GPUI's
    /// executors run on.
    fn with_env<T>(
        &self,
        body: impl FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut guard = self
            .vm
            .attach_current_thread()
            .context("attaching the current thread to the JavaVM")?;
        let activity = self.activity.as_obj();
        let result = body(&mut guard, activity);
        // A pending Java exception would poison every later call on this
        // thread, so clear it here and fold it into the error instead.
        if guard.exception_check().unwrap_or(false) {
            let _ = guard.exception_clear();
            return Err(anyhow!("the Java side threw while handling a host call"));
        }
        result
    }

    fn call_void(&self, name: &'static str) {
        let outcome = self.with_env(|env, activity| {
            env.call_method(activity, name, "()V", &[])
                .with_context(|| format!("calling {name}()"))?;
            Ok(())
        });
        // Void notifications have no caller who could act on a failure, so log
        // rather than propagate. Silence here would make a dead Java side look
        // like a hung Rust side.
        if let Err(error) = outcome {
            log::error!("tcode-android: {name} failed: {error:#}");
        }
    }

    fn call_bool_arg(&self, name: &'static str, value: bool) {
        let outcome = self.with_env(|env, activity| {
            env.call_method(activity, name, "(Z)V", &[JValue::Bool(u8::from(value))])
                .with_context(|| format!("calling {name}({value})"))?;
            Ok(())
        });
        if let Err(error) = outcome {
            log::error!("tcode-android: {name} failed: {error:#}");
        }
    }

    fn call_string_arg(&self, name: &'static str, value: &str) -> Result<()> {
        self.with_env(|env, activity| {
            let argument = env
                .new_string(value)
                .with_context(|| format!("allocating the {name} argument"))?;
            env.call_method(
                activity,
                name,
                "(Ljava/lang/String;)V",
                &[JValue::Object(&argument)],
            )
            .with_context(|| format!("calling {name}(..)"))?;
            Ok(())
        })
    }

    /// Called by the JNI frame entry point once Choreographer has fired, so the
    /// next `request_frame` posts a fresh callback.
    pub(crate) fn clear_frame_pending(&self) {
        self.frame_pending.store(false, Ordering::Release);
    }
}

impl AndroidHost for JniHost {
    fn wake_main_thread(&self) {
        self.call_void(method::WAKE);
    }

    fn request_frame(&self) {
        // Only the transition from not-pending to pending posts a callback.
        if self
            .frame_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.call_void(method::REQUEST_FRAME);
        }
    }

    fn finish_activity(&self) {
        self.call_void(method::FINISH);
    }

    fn open_uri(&self, uri: &str) -> Result<()> {
        self.call_string_arg(method::OPEN_URI, uri)
    }

    fn open_path(&self, path: &Path) -> Result<()> {
        // Android cannot hand a raw filesystem path to another app; sharing a
        // file needs a FileProvider content:// URI, which is app-configuration
        // rather than something this bridge can synthesize. Failing honestly is
        // better than opening the wrong thing.
        Err(anyhow!(
            "opening a filesystem path is unsupported on Android without a FileProvider: {}",
            path.display()
        ))
    }

    fn read_clipboard_text(&self) -> Result<Option<String>> {
        self.with_env(|env, activity| {
            let value = env
                .call_method(
                    activity,
                    method::READ_CLIPBOARD,
                    "()Ljava/lang/String;",
                    &[],
                )
                .context("calling readClipboardText()")?
                .l()
                .context("readClipboardText must return a String or null")?;
            if value.is_null() {
                return Ok(None);
            }
            let text: String = env
                .get_string(&value.into())
                .context("decoding the clipboard string")?
                .into();
            Ok(Some(text))
        })
    }

    fn write_clipboard_text(&self, text: &str) -> Result<()> {
        self.call_string_arg(method::WRITE_CLIPBOARD, text)
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Result<()> {
        // Deliberately unimplemented rather than stubbed to success: a
        // credential store that silently discards secrets is worse than one
        // that says it is absent, because the caller would believe the secret
        // was saved. Needs an Android Keystore-backed host service.
        Err(anyhow!("credential storage is not wired up on Android yet"))
    }

    fn read_credentials(&self, _url: &str) -> Result<Option<(String, Vec<u8>)>> {
        // `None` is honest here: nothing was ever stored, so nothing is found.
        Ok(None)
    }

    fn delete_credentials(&self, _url: &str) -> Result<()> {
        Err(anyhow!("credential storage is not wired up on Android yet"))
    }

    fn show_soft_keyboard(&self) {
        self.call_void(method::SHOW_KEYBOARD);
    }

    fn hide_soft_keyboard(&self) {
        self.call_void(method::HIDE_KEYBOARD);
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // Positioning the IME needs an InputConnection reporting caret bounds
        // through CursorAnchorInfo. Until that exists the platform IME places
        // itself, which is wrong but not broken.
    }

    fn text_input_state_changed(&self, _change: TextInputStateChange) {
        // Same dependency as `update_ime_position`: without an InputConnection
        // there is nothing on the Java side to tell.
    }

    fn set_back_enabled(&self, enabled: bool) {
        self.call_bool_arg(method::SET_BACK_ENABLED, enabled);
    }

    fn window_appearance(&self) -> WindowAppearance {
        let dark = self
            .with_env(|env, activity| {
                let value = env
                    .call_method(activity, method::IS_DARK_MODE, "()Z", &[])
                    .context("calling isDarkMode()")?
                    .z()
                    .context("isDarkMode must return a boolean")?;
                Ok(value)
            })
            .unwrap_or(false);
        if dark {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        }
    }

    fn thermal_state(&self) -> ThermalState {
        // Android exposes this through PowerManager's thermal status listener,
        // which is a Java-side subscription rather than a query. Reporting
        // nominal is the honest default until that listener exists; guessing
        // would make GPUI throttle for no reason.
        ThermalState::Nominal
    }
}
