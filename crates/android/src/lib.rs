//! Android cdylib entry point loaded by the NativeActivity host.

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub fn android_main(app: android_activity::AndroidApp) {
    use futures::{StreamExt as _, channel::mpsc};

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("Tcode-GPUI"),
    );
    std::panic::set_hook(Box::new(|panic| log::error!("GPUI Android panic: {panic}")));
    gpui_android::init_platform(&app);
    gpui::Application::with_platform(gpui_android::platform()).run(|cx| {
        tcode_mobile::run(cx);
        let (back_sender, mut back_receiver) = mpsc::unbounded();
        gpui_android::set_back_callback(move || {
            let _ = back_sender.unbounded_send(());
        });
        cx.spawn(async move |cx| {
            while back_receiver.next().await.is_some() {
                cx.update(|cx| {
                    if !tcode_mobile::handle_back(cx) {
                        cx.quit();
                    }
                });
            }
        })
        .detach();
    });
}

#[cfg(target_os = "android")]
mod jni_exports {
    use jni::{
        JNIEnv,
        objects::{JObject, JString},
        sys::{jboolean, jint},
    };

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeCommitText(
        mut env: JNIEnv,
        _activity: JObject,
        text: JString,
    ) {
        if let Ok(text) = env.get_string(&text) {
            gpui_android::jni_commit_text(text.into());
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeSetComposingText(
        mut env: JNIEnv,
        _activity: JObject,
        text: JString,
    ) {
        if let Ok(text) = env.get_string(&text) {
            gpui_android::jni_set_composing_text(text.into());
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeFinishComposingText(
        _env: JNIEnv,
        _activity: JObject,
    ) {
        gpui_android::jni_finish_composing_text();
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeDeleteBackward(
        _env: JNIEnv,
        _activity: JObject,
    ) {
        gpui_android::jni_delete_backward();
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeKeyEvent(
        _env: JNIEnv,
        _activity: JObject,
        key_code: jint,
        down: jboolean,
        unicode_code_point: jint,
        meta_state: jint,
    ) {
        gpui_android::jni_key_event(key_code, down != 0, unicode_code_point, meta_state);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeOnInsets(
        _env: JNIEnv,
        _activity: JObject,
        left: jint,
        top: jint,
        right: jint,
        bottom: jint,
        ime_bottom: jint,
    ) {
        gpui_android::jni_on_insets(left, top, right, bottom, ime_bottom);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeOnBack(
        _env: JNIEnv,
        _activity: JObject,
        enabled: jboolean,
    ) {
        if enabled != 0 {
            gpui_android::jni_on_back();
        }
    }
}
