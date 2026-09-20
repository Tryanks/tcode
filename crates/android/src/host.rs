//! Android services behind this client's `ClientHost`.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use android_activity::AndroidApp;
use futures::{StreamExt as _, channel::mpsc};
use gpui::App;
use jni::{
    Env, JavaVM, jni_sig, jni_str,
    objects::{JObject, JString, JValue},
    refs::Global,
};
use tcode_client::host::HostFuture;
use tcode_remote::NativeClientHost;

const RESULT_OK: i32 = 0;
const RESULT_CANCELLED: i32 = 1;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static EVENT_SENDER: LazyLock<Mutex<Option<mpsc::UnboundedSender<BridgeEvent>>>> =
    LazyLock::new(|| Mutex::new(None));

struct BridgeEvent {
    request_id: u64,
    status: i32,
    value: Option<String>,
}

#[derive(Clone)]
struct JniObject {
    vm: JavaVM,
    activity: Arc<Global<JObject<'static>>>,
}

impl JniObject {
    fn call_string(&self, method: &'static jni::strings::JNIStr) -> Result<Option<String>, String> {
        self.with_env(|env, activity| {
            let object = env
                .call_method(activity, method, jni_sig!("()Ljava/lang/String;"), &[])?
                .l()?;
            if object.is_null() {
                return Ok(None);
            }
            JString::cast_local(env, object)?
                .try_to_string(env)
                .map(Some)
        })
    }

    fn with_env<T>(
        &self,
        callback: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<T>,
    ) -> Result<T, String> {
        // A permanent attachment is a no-op on threads the JVM already owns.
        self.vm
            .attach_current_thread(|env| {
                callback(env, self.activity.as_obj()).inspect_err(|_| {
                    if env.exception_check() {
                        env.exception_describe();
                        env.exception_clear();
                    }
                })
            })
            .map_err(|error: jni::errors::Error| error.to_string())
    }
}

#[derive(Clone)]
struct JavaBridge {
    app: AndroidApp,
    object: JniObject,
}

impl JavaBridge {
    fn new(app: AndroidApp) -> Result<Self, String> {
        // SAFETY: Android owns the VM and activity for the NativeActivity process lifetime.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        let activity = vm
            .attach_current_thread(|env| {
                // SAFETY: `activity_as_ptr` is the live GpuiActivity reference.
                let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
                env.new_global_ref(&activity)
            })
            .map_err(|error: jni::errors::Error| {
                format!("failed retaining GpuiActivity: {error}")
            })?;
        Ok(Self {
            app,
            object: JniObject {
                vm,
                activity: Arc::new(activity),
            },
        })
    }

    fn set_app_background_dark(&self, dark: bool) {
        let object = self.object.clone();
        self.app.run_on_java_main_thread(Box::new(move || {
            if let Err(error) = object.with_env(|env, activity| {
                env.call_method(
                    activity,
                    jni_str!("gpuiSetAppBackgroundDark"),
                    jni_sig!("(Z)V"),
                    &[JValue::Bool(dark)],
                )?;
                Ok(())
            }) {
                log::error!("Android system bar appearance JNI call failed: {error}");
            }
        }));
    }

    fn start_camera(&self, request_id: u64) {
        let object = self.object.clone();
        self.app.run_on_java_main_thread(Box::new(move || {
            if let Err(error) = object.with_env(|env, activity| {
                env.call_method(
                    activity,
                    jni_str!("gpuiStartCameraScan"),
                    jni_sig!("(J)V"),
                    &[JValue::Long(request_id as i64)],
                )?;
                Ok(())
            }) {
                log::error!("Android camera JNI call failed: {error}");
                deliver_result(request_id, 2, Some(error));
            }
        }));
    }
}

pub(crate) fn native_host(
    app: AndroidApp,
    cx: &mut App,
) -> Result<(NativeClientHost, Option<String>), String> {
    let bridge = JavaBridge::new(app)?;
    let appearance_bridge = bridge.clone();
    // Keep system chrome in sync with explicit app themes as well as system mode.
    cx.observe_global::<tcode_ui::theme::Theme>(move |cx| {
        appearance_bridge
            .set_app_background_dark(cx.global::<tcode_ui::theme::Theme>().mode.is_dark());
    })
    .detach();
    let data_dir = bridge
        .object
        .call_string(jni_str!("gpuiDataDir"))?
        .map(PathBuf::from)
        .ok_or_else(|| "Android filesDir is unavailable".to_string())?;
    let device_name = bridge
        .object
        .call_string(jni_str!("gpuiDeviceModel"))?
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Android".into());
    let platform = bridge
        .object
        .call_string(jni_str!("gpuiDevicePlatform"))?
        .filter(|platform| !platform.trim().is_empty())
        .unwrap_or_else(|| "Android".into());
    let system_locale = bridge
        .object
        .call_string(jni_str!("gpuiSystemLocale"))?
        .filter(|locale| !locale.trim().is_empty());

    let callbacks = Rc::new(RefCell::new(HashMap::<
        u64,
        async_channel::Sender<Result<String, String>>,
    >::new()));
    let (sender, mut receiver) = mpsc::unbounded();
    *EVENT_SENDER.lock().expect("Android event sender poisoned") = Some(sender);
    let pending = callbacks.clone();
    cx.spawn(async move |cx| {
        while let Some(event) = receiver.next().await {
            let pending = pending.clone();
            cx.update(move |_cx| {
                let sender = pending.borrow_mut().remove(&event.request_id);
                let Some(sender) = sender else {
                    log::warn!(
                        "received result for unknown Android camera request {}",
                        event.request_id
                    );
                    return;
                };
                let result = match (event.status, event.value) {
                    (RESULT_OK, Some(value)) if !value.is_empty() => Ok(value),
                    (RESULT_CANCELLED, value) => Err(value.unwrap_or_else(|| "已取消扫描".into())),
                    (_, value) => Err(value.unwrap_or_else(|| "Android 相机扫描失败".into())),
                };
                let _ = sender.try_send(result);
            });
        }
    })
    .detach();

    let multicast = bridge.object.clone();
    let camera = bridge.clone();
    let host = NativeClientHost::new(data_dir, device_name)
        .with_platform(platform)
        .with_multicast_lock(move |acquire| {
            if multicast
                .with_env(|env, activity| {
                    env.call_method(
                        activity,
                        jni_str!("gpuiMulticastLock"),
                        jni_sig!("(Z)V"),
                        &[JValue::Bool(acquire)],
                    )?;
                    Ok(())
                })
                .is_err()
            {
                log::warn!("Android multicast lock unavailable");
            }
        })
        .with_qr_scanner(move || -> HostFuture<'static, Result<String, String>> {
            let (sender, receiver) = async_channel::bounded(1);
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            callbacks.borrow_mut().insert(request_id, sender);
            camera.start_camera(request_id);
            Box::pin(async move {
                receiver
                    .recv()
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
            })
        });
    Ok((host, system_locale))
}

pub(crate) fn deliver_result(request_id: u64, status: i32, value: Option<String>) {
    let sender = EVENT_SENDER
        .lock()
        .expect("Android event sender poisoned")
        .clone();
    if let Some(sender) = sender {
        let _ = sender.unbounded_send(BridgeEvent {
            request_id,
            status,
            value,
        });
    } else {
        log::warn!("dropping Android camera result before host initialization");
    }
}
