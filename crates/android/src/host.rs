//! Android services used by `tcode-mobile`.

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
    JNIEnv, JavaVM,
    objects::{GlobalRef, JObject, JString, JValue},
};
use tcode_mobile::host::{NativeHost, ScanDone};

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
    vm: Arc<JavaVM>,
    activity: GlobalRef,
}

impl JniObject {
    fn call_string(&self, method: &str) -> Result<Option<String>, String> {
        self.with_env(|env, activity| {
            let object = env
                .call_method(activity, method, "()Ljava/lang/String;", &[])?
                .l()?;
            if object.is_null() {
                return Ok(None);
            }
            Ok(Some(env.get_string(&JString::from(object))?.into()))
        })
    }

    fn with_env<T>(
        &self,
        callback: impl FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> jni::errors::Result<T>,
    ) -> Result<T, String> {
        let mut env = self
            .vm
            .attach_current_thread()
            .map_err(|error| format!("failed attaching Android JVM thread: {error}"))?;
        callback(&mut env, self.activity.as_obj()).map_err(|error| {
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
            error.to_string()
        })
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
        let vm = Arc::new(
            unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }
                .map_err(|error| format!("failed accessing Android VM: {error}"))?,
        );
        let env = vm
            .attach_current_thread()
            .map_err(|error| format!("failed attaching Android host thread: {error}"))?;
        // SAFETY: `activity_as_ptr` is the live GpuiActivity local reference.
        let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
        let activity = env
            .new_global_ref(activity)
            .map_err(|error| format!("failed retaining GpuiActivity: {error}"))?;
        drop(env);
        Ok(Self {
            app,
            object: JniObject { vm, activity },
        })
    }

    fn start_camera(&self, request_id: u64) {
        let object = self.object.clone();
        self.app.run_on_java_main_thread(Box::new(move || {
            if let Err(error) = object.with_env(|env, activity| {
                env.call_method(
                    activity,
                    "gpuiStartCameraScan",
                    "(J)V",
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

pub(crate) fn native_host(app: AndroidApp, cx: &mut App) -> Result<NativeHost, String> {
    let bridge = JavaBridge::new(app)?;
    let data_dir = bridge
        .object
        .call_string("gpuiDataDir")?
        .map(PathBuf::from)
        .ok_or_else(|| "Android filesDir is unavailable".to_string())?;
    let device_name = bridge
        .object
        .call_string("gpuiDeviceModel")?
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Android".into());

    let callbacks = Rc::new(RefCell::new(HashMap::<u64, ScanDone>::new()));
    let (sender, mut receiver) = mpsc::unbounded();
    *EVENT_SENDER.lock().expect("Android event sender poisoned") = Some(sender);
    let pending = callbacks.clone();
    cx.spawn(async move |cx| {
        while let Some(event) = receiver.next().await {
            let pending = pending.clone();
            cx.update(move |cx| {
                let callback = pending.borrow_mut().remove(&event.request_id);
                let Some(callback) = callback else {
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
                callback(result, cx);
            });
        }
    })
    .detach();

    let multicast = bridge.object.clone();
    let camera = bridge.clone();
    Ok(NativeHost::new(data_dir, device_name)
        .with_multicast_lock(move |acquire| {
            if multicast
                .with_env(|env, activity| {
                    env.call_method(
                        activity,
                        "gpuiMulticastLock",
                        "(Z)V",
                        &[JValue::Bool(acquire.into())],
                    )?;
                    Ok(())
                })
                .is_err()
            {
                log::warn!("Android multicast lock unavailable");
            }
        })
        .with_qr_scanner(move |done, _cx| {
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            callbacks.borrow_mut().insert(request_id, done);
            camera.start_camera(request_id);
        }))
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
