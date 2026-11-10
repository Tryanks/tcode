use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fmt;
use std::ptr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation::base::{
    CFEqual, CFGetTypeID, CFHash, CFRelease, CFRetain, CFTypeID, CFTypeRef, TCFType,
};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::runloop::{
    CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRun, CFRunLoopSourceRef, CFRunLoopWakeUp,
    kCFRunLoopDefaultMode,
};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use super::super::{BackendError, BackendErrorCode, RootInfo, RootKind};
use crate::outline::{Frame, UiNode, canonical_role, is_text_sparse};

type AXUIElementRef = CFTypeRef;
type AXValueRef = CFTypeRef;
type AXObserverRef = CFTypeRef;
type AXError = i32;
type AXValueType = u32;

const AX_SUCCESS: AXError = 0;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
const AX_ERROR_API_DISABLED: AXError = -25211;
const AX_ERROR_NO_VALUE: AXError = -25212;
const AX_VALUE_CGPOINT: AXValueType = 1;
const AX_VALUE_CGSIZE: AXValueType = 2;
const AX_VALUE_CGRECT: AXValueType = 3;
const MAX_NODES: usize = 3_000;
const MAX_CHILDREN_PER_NODE: usize = 500;

static ACTIVATED_PIDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
static WEB_SPARSE_PIDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
static AX_OBSERVERS: OnceLock<Mutex<HashMap<u32, usize>>> = OnceLock::new();

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementGetTypeID() -> CFTypeID;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementGetAttributeValueCount(
        element: AXUIElementRef,
        attribute: CFStringRef,
        count: *mut isize,
    ) -> AXError;
    fn AXUIElementCopyAttributeValues(
        element: AXUIElementRef,
        attribute: CFStringRef,
        index: isize,
        max_values: isize,
        values: *mut CFArrayRef,
    ) -> AXError;
    fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFArrayRef) -> AXError;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXValueGetTypeID() -> CFTypeID;
    fn AXValueGetType(value: AXValueRef) -> AXValueType;
    fn AXValueGetValue(value: AXValueRef, value_type: AXValueType, value_ptr: *mut c_void) -> bool;
    fn AXObserverCreateWithInfoCallback(
        pid: i32,
        callback: extern "C" fn(AXObserverRef, AXUIElementRef, CFStringRef, CFTypeRef, *mut c_void),
        observer: *mut AXObserverRef,
    ) -> AXError;
    fn AXObserverGetRunLoopSource(observer: AXObserverRef) -> CFRunLoopSourceRef;
    fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        refcon: *mut c_void,
    ) -> AXError;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
    fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
    fn objc_msgSend();
}

struct OwnedCf(CFTypeRef);

impl OwnedCf {
    fn as_ax(&self) -> AXUIElementRef {
        self.0
    }

    unsafe fn from_create(value: CFTypeRef) -> Option<Self> {
        (!value.is_null()).then_some(Self(value))
    }

    unsafe fn from_borrowed(value: CFTypeRef) -> Option<Self> {
        if value.is_null() {
            None
        } else {
            // SAFETY: the caller supplied a live CF object borrowed from a
            // container; retaining gives this wrapper independent ownership.
            unsafe { CFRetain(value) };
            Some(Self(value))
        }
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        // SAFETY: OwnedCf is constructed only for create-rule or retained refs.
        unsafe { CFRelease(self.0) };
    }
}

#[derive(Debug)]
pub(super) struct AxFailure {
    operation: &'static str,
    code: AXError,
}

impl fmt::Display for AxFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed with AX error {} ({})",
            self.operation,
            self.code,
            ax_error_name(self.code)
        )
    }
}

pub(super) struct Target {
    _application: OwnedCf,
    element: OwnedCf,
}

impl Target {
    pub(super) fn press(&self) -> Result<(), AxFailure> {
        let action = CFString::new("AXPress");
        let code =
            unsafe { AXUIElementPerformAction(self.element.as_ax(), action.as_concrete_TypeRef()) };
        ax_result("AXPress", code)
    }

    pub(super) fn set_text(&self, text: &str) -> Result<(), AxFailure> {
        let attribute = CFString::new("AXValue");
        let value = CFString::new(text);
        let code = unsafe {
            AXUIElementSetAttributeValue(
                self.element.as_ax(),
                attribute.as_concrete_TypeRef(),
                value.as_CFTypeRef(),
            )
        };
        ax_result("setting AXValue", code)
    }

    pub(super) fn focus(&self) -> Result<(), AxFailure> {
        let attribute = CFString::new("AXFocused");
        let code = unsafe {
            AXUIElementSetAttributeValue(
                self.element.as_ax(),
                attribute.as_concrete_TypeRef(),
                CFBoolean::true_value().as_CFTypeRef(),
            )
        };
        ax_result("setting AXFocused", code)
    }

    pub(super) fn frame(&self) -> Frame {
        element_frame(self.element.as_ax())
    }
}

/// The bundle identifier of the application running as `pid`; `None` for a
/// process without one.
pub(super) fn bundle_identifier(pid: u32) -> Option<String> {
    // SAFETY: all selectors and return types below are stable Foundation/AppKit
    // APIs. The autorelease pool bounds temporary Objective-C objects created
    // on the MCP runtime thread.
    unsafe {
        let pool_class = objc_getClass(c"NSAutoreleasePool".as_ptr());
        let application_class = objc_getClass(c"NSRunningApplication".as_ptr());
        if pool_class.is_null() || application_class.is_null() {
            return None;
        }
        let pool = send_id(pool_class, c"new");
        let application = send_id_i32(
            application_class,
            c"runningApplicationWithProcessIdentifier:",
            pid as i32,
        );
        let result = if application.is_null() {
            None
        } else {
            let identifier = send_id(application, c"bundleIdentifier");
            if identifier.is_null() {
                None
            } else {
                let utf8 = send_id(identifier, c"UTF8String") as *const std::ffi::c_char;
                (!utf8.is_null()).then(|| {
                    std::ffi::CStr::from_ptr(utf8)
                        .to_string_lossy()
                        .into_owned()
                })
            }
        };
        send_void(pool, c"drain");
        result
    }
}

pub(super) fn frontmost_application_pid() -> Option<u32> {
    // SAFETY: all selectors and return types below are stable AppKit APIs. The
    // autorelease pool bounds temporary Objective-C objects on this thread.
    unsafe {
        let pool_class = objc_getClass(c"NSAutoreleasePool".as_ptr());
        let workspace_class = objc_getClass(c"NSWorkspace".as_ptr());
        if pool_class.is_null() || workspace_class.is_null() {
            return None;
        }
        let pool = send_id(pool_class, c"new");
        let workspace = send_id(workspace_class, c"sharedWorkspace");
        let application = send_id(workspace, c"frontmostApplication");
        let pid = (!application.is_null()).then(|| send_i32(application, c"processIdentifier"));
        send_void(pool, c"drain");
        pid.filter(|pid| *pid > 0)
            .and_then(|pid| u32::try_from(pid).ok())
    }
}

unsafe fn send_id(receiver: *mut c_void, selector: &std::ffi::CStr) -> *mut c_void {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { send(receiver, sel_registerName(selector.as_ptr())) }
}

unsafe fn send_id_i32(receiver: *mut c_void, selector: &std::ffi::CStr, value: i32) -> *mut c_void {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void, i32) -> *mut c_void =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { send(receiver, sel_registerName(selector.as_ptr()), value) }
}

unsafe fn send_i32(receiver: *mut c_void, selector: &std::ffi::CStr) -> i32 {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { send(receiver, sel_registerName(selector.as_ptr())) }
}

unsafe fn send_void(receiver: *mut c_void, selector: &std::ffi::CStr) {
    let send: unsafe extern "C" fn(*mut c_void, *mut c_void) =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { send(receiver, sel_registerName(selector.as_ptr())) }
}

pub(super) fn root_kind(root: &RootInfo) -> RootKind {
    let Ok((_application, window)) = locate_window(root) else {
        return RootKind::Window;
    };
    let role = attribute_string(window.as_ax(), "AXRole").unwrap_or_default();
    let subrole = attribute_string(window.as_ax(), "AXSubrole").unwrap_or_default();
    let combined = format!("{role} {subrole}").to_ascii_lowercase();
    if combined.contains("sheet") {
        RootKind::Sheet
    } else if combined.contains("dialog") || combined.contains("systemdialog") {
        RootKind::Dialog
    } else if combined.contains("popover") {
        RootKind::Popover
    } else if combined.contains("menu") {
        RootKind::Menu
    } else {
        RootKind::Window
    }
}

fn should_activate_chromium(root: &RootInfo) -> bool {
    is_chromium_bundle(&root.bundle_id) || lock_set(&WEB_SPARSE_PIDS).contains(&root.pid)
}

fn is_chromium_bundle(bundle_id: &str) -> bool {
    let bundle_id = bundle_id.to_ascii_lowercase();
    ["chrome", "chromium", "electron"]
        .into_iter()
        .any(|marker| bundle_id.contains(marker))
}

fn activate_chromium_accessibility(pid: u32, application: AXUIElementRef) {
    for attribute_name in ["AXManualAccessibility", "AXEnhancedUserInterface"] {
        let attribute = CFString::new(attribute_name);
        // SAFETY: application is a live AX application element and both the
        // attribute string and kCFBooleanTrue remain live for this call.
        let code = unsafe {
            AXUIElementSetAttributeValue(
                application,
                attribute.as_concrete_TypeRef(),
                CFBoolean::true_value().as_CFTypeRef(),
            )
        };
        if code != AX_SUCCESS {
            log::debug!(
                "Chromium AX activation attribute {attribute_name} was rejected for pid {pid}: {}",
                ax_error_name(code)
            );
        }
    }

    let first_activation = lock_set(&ACTIVATED_PIDS).insert(pid);
    if !first_activation {
        return;
    }
    register_chromium_observer(pid, application);
    std::thread::sleep(Duration::from_millis(300));
}

fn register_chromium_observer(pid: u32, application: AXUIElementRef) {
    let Ok(pid_i32) = i32::try_from(pid) else {
        log::debug!("Chromium AX observer pid {pid} is out of range");
        return;
    };
    let mut observer: AXObserverRef = ptr::null();
    // SAFETY: the output pointer is valid, the callback has the verified AX
    // ABI, and a null/failed observer is handled without dereferencing it.
    let code = unsafe {
        AXObserverCreateWithInfoCallback(pid_i32, chromium_observer_callback, &mut observer)
    };
    if code != AX_SUCCESS || observer.is_null() {
        log::debug!(
            "could not create Chromium AX observer for pid {pid}: {}",
            ax_error_name(code)
        );
        return;
    }

    for notification_name in CHROMIUM_NOTIFICATIONS {
        let notification = CFString::new(notification_name);
        let code =
            add_chromium_notification(observer, application, notification.as_concrete_TypeRef());
        if code != AX_SUCCESS {
            log::debug!(
                "Chromium AX notification {notification_name} was rejected for pid {pid}: {}",
                ax_error_name(code)
            );
        }
    }

    // SAFETY: observer is a live create-rule AXObserver returned above; the
    // borrowed run-loop source remains live while the observer is retained.
    let source = unsafe { AXObserverGetRunLoopSource(observer) };
    if source.is_null() || add_observer_run_loop_source(source).is_none() {
        log::debug!("could not attach Chromium AX observer for pid {pid} to its run loop");
        // SAFETY: observer is the create-rule object returned above and has not
        // been handed to the persistent registry on this failure path.
        unsafe { CFRelease(observer) };
        return;
    }

    observer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(pid, observer as usize);
}

const CHROMIUM_NOTIFICATIONS: [&str; 13] = [
    "AXFocusedUIElementChanged",
    "AXFocusedWindowChanged",
    "AXApplicationActivated",
    "AXApplicationDeactivated",
    "AXApplicationHidden",
    "AXApplicationShown",
    "AXWindowCreated",
    "AXWindowMoved",
    "AXWindowResized",
    "AXValueChanged",
    "AXTitleChanged",
    "AXSelectedChildrenChanged",
    "AXLayoutChanged",
];

extern "C" fn chromium_observer_callback(
    _observer: AXObserverRef,
    _element: AXUIElementRef,
    _notification: CFStringRef,
    _info: CFTypeRef,
    _refcon: *mut c_void,
) {
}

fn add_chromium_notification(
    observer: AXObserverRef,
    application: AXUIElementRef,
    notification: CFStringRef,
) -> AXError {
    // SAFETY: observer, application, and notification are live values using
    // the public AXObserverAddNotification ABI; null refcon is permitted.
    unsafe { AXObserverAddNotification(observer, application, notification, ptr::null_mut()) }
}

struct ObserverRunLoop {
    run_loop: usize,
    first_source: usize,
}

fn add_observer_run_loop_source(source: CFRunLoopSourceRef) -> Option<()> {
    static RUN_LOOP: OnceLock<Option<ObserverRunLoop>> = OnceLock::new();
    let runtime = RUN_LOOP
        .get_or_init(|| start_observer_run_loop(source))
        .as_ref()?;
    if runtime.first_source != source as usize {
        // SAFETY: the persistent run loop and borrowed AXObserver source are
        // live; CFRunLoopAddSource retains the source in the default mode.
        unsafe {
            CFRunLoopAddSource(
                runtime.run_loop as core_foundation::runloop::CFRunLoopRef,
                source,
                kCFRunLoopDefaultMode,
            );
            CFRunLoopWakeUp(runtime.run_loop as core_foundation::runloop::CFRunLoopRef);
        }
    }
    Some(())
}

fn start_observer_run_loop(source: CFRunLoopSourceRef) -> Option<ObserverRunLoop> {
    let source_address = source as usize;
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("tcode-cu-ax-observer".into())
        .spawn(move || {
            // SAFETY: this call obtains the current dedicated thread's live
            // Core Foundation run loop.
            let run_loop = unsafe { CFRunLoopGetCurrent() };
            // SAFETY: source_address is the live AXObserver source passed into
            // this thread setup, and the default mode is a static CF value.
            unsafe {
                CFRunLoopAddSource(
                    run_loop,
                    source_address as CFRunLoopSourceRef,
                    kCFRunLoopDefaultMode,
                );
            }
            if ready_tx.send(run_loop as usize).is_err() {
                return;
            }
            // SAFETY: this dedicated thread owns and runs its current run loop
            // for the process lifetime to service persistent AX observers.
            unsafe { CFRunLoopRun() };
            log::debug!("persistent Chromium AX observer run loop stopped unexpectedly");
        });
    if let Err(error) = thread {
        log::debug!("could not spawn Chromium AX observer run-loop thread: {error}");
        return None;
    }
    match ready_rx.recv() {
        Ok(run_loop) => Some(ObserverRunLoop {
            run_loop,
            first_source: source_address,
        }),
        Err(error) => {
            log::debug!("Chromium AX observer run-loop thread failed during setup: {error}");
            None
        }
    }
}

fn lock_set(
    cell: &'static OnceLock<Mutex<HashSet<u32>>>,
) -> std::sync::MutexGuard<'static, HashSet<u32>> {
    cell.get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn observer_registry() -> &'static Mutex<HashMap<u32, usize>> {
    AX_OBSERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn observe_tree(root: &RootInfo) -> Result<UiNode, BackendError> {
    let application = create_application(root.pid).ok_or_else(|| {
        BackendError::new(
            BackendErrorCode::RootNotFound,
            format!("could not create an AX application for pid {}", root.pid),
        )
    })?;
    if should_activate_chromium(root) {
        activate_chromium_accessibility(root.pid, application.as_ax());
    }
    let window = locate_window_in_application(application.as_ax(), root)?;
    let mut context = WalkContext {
        count: 0,
        visited: VisitedElements::default(),
        root_frame: root.frame,
    };
    let mut tree = walk_tree(window.as_ax(), &mut context).ok_or_else(|| {
        BackendError::new(
            BackendErrorCode::ObservationFailed,
            format!("the accessibility tree for {} was empty", root.title),
        )
    })?;
    if tree.title.is_empty() {
        tree.title.clone_from(&root.title);
    }
    if !tree.frame.has_area() {
        tree.frame = root.frame;
    }
    if is_text_sparse(&tree) {
        lock_set(&WEB_SPARSE_PIDS).insert(root.pid);
    }
    Ok(tree)
}

pub(super) fn locate_target(
    root: &RootInfo,
    path: &[usize],
    expected_role: Option<&str>,
    expected_title: Option<&str>,
) -> Result<Target, BackendError> {
    let (application, mut element) = locate_window(root)?;
    for &index in path {
        let children = copy_children(element.as_ax(), index.saturating_add(1)).map_err(|code| {
            BackendError::new(
                BackendErrorCode::OperationFailed,
                format!(
                    "could not read the children along the target's accessibility path: {}",
                    ax_error_text(code)
                ),
            )
        })?;
        let Some(child) = children.into_iter().nth(index) else {
            return Err(BackendError::new(
                BackendErrorCode::OperationFailed,
                "the target's accessibility path moved; call observe_ui again",
            ));
        };
        element = child;
    }

    if let Some(expected_role) = expected_role {
        let actual = attribute_string(element.as_ax(), "AXRole").unwrap_or_default();
        if canonical_role(&actual) != canonical_role(expected_role) {
            return Err(BackendError::new(
                BackendErrorCode::OperationFailed,
                format!(
                    "target role changed from {} to {}; call observe_ui again",
                    canonical_role(expected_role),
                    canonical_role(&actual)
                ),
            ));
        }
    }
    if let Some(expected_title) = expected_title.filter(|title| !title.is_empty()) {
        let actual = attribute_string(element.as_ax(), "AXTitle").unwrap_or_default();
        if !actual.is_empty() && actual != expected_title {
            return Err(BackendError::new(
                BackendErrorCode::OperationFailed,
                "target title changed; call observe_ui again",
            ));
        }
    }
    Ok(Target {
        _application: application,
        element,
    })
}

fn create_application(pid: u32) -> Option<OwnedCf> {
    let application = unsafe { AXUIElementCreateApplication(pid as i32) };
    // SAFETY: AXUIElementCreateApplication follows the create rule.
    unsafe { OwnedCf::from_create(application) }
}

fn locate_window(root: &RootInfo) -> Result<(OwnedCf, OwnedCf), BackendError> {
    let application = create_application(root.pid).ok_or_else(|| {
        BackendError::new(
            BackendErrorCode::RootNotFound,
            format!("could not create an AX application for pid {}", root.pid),
        )
    })?;
    let window = locate_window_in_application(application.as_ax(), root)?;
    Ok((application, window))
}

fn locate_window_in_application(
    application: AXUIElementRef,
    root: &RootInfo,
) -> Result<OwnedCf, BackendError> {
    // The best-scoring window of the process would otherwise stand in for
    // a closed one under the closed one's identity.
    if !super::window_exists(root) {
        return Err(BackendError::new(
            BackendErrorCode::RootNotFound,
            format!(
                "the window of root {} is no longer open; call find_roots again",
                root.ref_id
            ),
        ));
    }
    let windows = try_copy_attribute_elements(application, "AXWindows", 200).map_err(|code| {
        let message = if code == AX_ERROR_API_DISABLED {
            format!(
                "the system refused Accessibility access for this Tcode process while listing the windows of root {} (AX error {code}); the grant is not in effect for this process even if System Settings lists it, which happens after Tcode's executable changed on disk, so restart Tcode",
                root.ref_id
            )
        } else {
            format!(
                "could not list the windows of root {} (AX error {code}); the app may still be launching or not responding, so call find_roots and observe_ui again",
                root.ref_id
            )
        };
        BackendError::new(BackendErrorCode::RootNotFound, message)
    })?;
    windows
        .into_iter()
        .max_by(|left, right| {
            window_match_score(left.as_ax(), root)
                .total_cmp(&window_match_score(right.as_ax(), root))
        })
        .ok_or_else(|| {
            BackendError::new(
                BackendErrorCode::RootNotFound,
                format!("no AX window matched root {}", root.ref_id),
            )
        })
}

fn window_match_score(window: AXUIElementRef, root: &RootInfo) -> f64 {
    let title = attribute_string(window, "AXTitle").unwrap_or_default();
    let frame = element_frame(window);
    let exact_title = if !root.title.is_empty() && title == root.title {
        1_000_000.0
    } else if title.to_lowercase().contains(&root.title.to_lowercase()) {
        100_000.0
    } else {
        0.0
    };
    let frame_distance = (frame.x - root.frame.x).abs()
        + (frame.y - root.frame.y).abs()
        + (frame.w - root.frame.w).abs()
        + (frame.h - root.frame.h).abs();
    exact_title - frame_distance
}

struct WalkContext {
    count: usize,
    visited: VisitedElements,
    root_frame: Frame,
}

/// The elements a walk has entered, compared as AX identities.
///
/// Each element is retained for the walk: a child array is released once
/// its subtree is walked, and the next array the system hands out can reuse
/// a freed element's address, so an address alone cannot stand for a node.
#[derive(Default)]
struct VisitedElements {
    buckets: HashMap<usize, Vec<OwnedCf>>,
}

impl VisitedElements {
    /// Records `element`; `false` when an equal element was already recorded.
    fn insert(&mut self, element: AXUIElementRef) -> bool {
        let value = element as CFTypeRef;
        // SAFETY: the caller holds `element` live for the call.
        let hash = unsafe { CFHash(value) };
        let bucket = self.buckets.entry(hash).or_default();
        if bucket
            .iter()
            .any(|seen| unsafe { CFEqual(seen.0, value) } != 0)
        {
            return false;
        }
        // SAFETY: `element` is live; retaining keeps it so for the walk.
        if let Some(owned) = unsafe { OwnedCf::from_borrowed(value) } {
            bucket.push(owned);
        }
        true
    }
}

/// The subtree under `root`, depth first, ending once [`MAX_NODES`] nodes are
/// read. The walk keeps its own stack: a web area's tree can be deeper than
/// the thread's stack would allow one frame per level.
fn walk_tree(root: AXUIElementRef, context: &mut WalkContext) -> Option<UiNode> {
    let mut stack = vec![read_element(root, context)?];
    loop {
        let (_, children) = stack.last_mut()?;
        let child = if context.count < MAX_NODES {
            children.next()
        } else {
            None
        };
        if let Some(child) = child {
            if let Some(entry) = read_element(child.as_ax(), context) {
                stack.push(entry);
            }
            continue;
        }
        let (node, _) = stack.pop()?;
        let Some((parent, _)) = stack.last_mut() else {
            return Some(node);
        };
        let invisible_leaf = node.children.is_empty()
            && node.children_error.is_none()
            && !node.is_interactive()
            && node.title.is_empty()
            && node.value.is_empty()
            && (!node.frame.has_area()
                || (context.root_frame.has_area() && !node.frame.intersects(context.root_frame)));
        if !invisible_leaf {
            parent.children.push(node);
        }
    }
}

/// One element's node, without its children yet, and the children to walk.
fn read_element(
    element: AXUIElementRef,
    context: &mut WalkContext,
) -> Option<(UiNode, std::vec::IntoIter<OwnedCf>)> {
    if context.count >= MAX_NODES || !context.visited.insert(element) {
        return None;
    }
    context.count += 1;
    let role = attribute_string(element, "AXRole").unwrap_or_else(|| "AXUnknown".into());
    let title = attribute_string(element, "AXTitle")
        .or_else(|| attribute_string(element, "AXLabel"))
        .unwrap_or_default();
    let value = attribute_text(element, "AXValue").unwrap_or_default();
    let description = ["AXDescription", "AXHelp", "AXRoleDescription"]
        .into_iter()
        .filter_map(|attribute| attribute_string(element, attribute))
        .find(|description| !description.is_empty() && description != &title)
        .unwrap_or_default();
    let frame = element_frame(element);
    let actions = action_names(element);
    let enabled = attribute_bool(element, "AXEnabled").unwrap_or(true);
    let focused = attribute_bool(element, "AXFocused").unwrap_or(false);
    let remaining = MAX_NODES - context.count;
    let (children, children_error) =
        match copy_children(element, remaining.min(MAX_CHILDREN_PER_NODE)) {
            Ok(children) => (children, None),
            Err(code) => (Vec::new(), Some(ax_error_text(code))),
        };
    let node = UiNode {
        ref_id: String::new(),
        role: canonical_role(&role),
        title,
        value,
        description,
        frame,
        actions,
        enabled,
        focused,
        children: Vec::new(),
        children_error,
    };
    Some((node, children.into_iter()))
}

/// An element that has no children attribute, or no value for it, has none.
fn copy_children(element: AXUIElementRef, maximum: usize) -> Result<Vec<OwnedCf>, AXError> {
    match try_copy_attribute_elements(element, "AXChildren", maximum) {
        Err(AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE) => Ok(Vec::new()),
        result => result,
    }
}

/// The elements of an attribute, or the AX error code of a failed copy.
fn try_copy_attribute_elements(
    element: AXUIElementRef,
    attribute_name: &str,
    maximum: usize,
) -> Result<Vec<OwnedCf>, AXError> {
    if maximum == 0 {
        return Ok(Vec::new());
    }
    let attribute = CFString::new(attribute_name);
    let mut count = 0_isize;
    let count_code = unsafe {
        AXUIElementGetAttributeValueCount(element, attribute.as_concrete_TypeRef(), &mut count)
    };
    if count_code != AX_SUCCESS {
        return Err(count_code);
    }
    if count <= 0 {
        return Ok(Vec::new());
    }
    let count = usize::try_from(count).unwrap_or(0).min(maximum);
    let mut array: CFArrayRef = ptr::null();
    let code = unsafe {
        AXUIElementCopyAttributeValues(
            element,
            attribute.as_concrete_TypeRef(),
            0,
            count as isize,
            &mut array,
        )
    };
    if code != AX_SUCCESS {
        return Err(code);
    }
    if array.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: CopyAttributeValues returns a create-rule CFArray.
    let Some(array_owner) = (unsafe { OwnedCf::from_create(array as CFTypeRef) }) else {
        return Ok(Vec::new());
    };
    let actual_count = unsafe { CFArrayGetCount(array) }.max(0) as usize;
    let mut values = Vec::with_capacity(actual_count);
    for index in 0..actual_count {
        let value = unsafe { CFArrayGetValueAtIndex(array, index as isize) } as CFTypeRef;
        if value.is_null() || unsafe { CFGetTypeID(value) } != unsafe { AXUIElementGetTypeID() } {
            continue;
        }
        // SAFETY: value is borrowed from array_owner, which remains live.
        if let Some(value) = unsafe { OwnedCf::from_borrowed(value) } {
            values.push(value);
        }
    }
    drop(array_owner);
    Ok(values)
}

fn copy_attribute(element: AXUIElementRef, attribute_name: &str) -> Option<OwnedCf> {
    let attribute = CFString::new(attribute_name);
    let mut value: CFTypeRef = ptr::null();
    let code = unsafe {
        AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef(), &mut value)
    };
    if code == AX_SUCCESS {
        // SAFETY: CopyAttributeValue returns a create-rule object on success.
        unsafe { OwnedCf::from_create(value) }
    } else {
        None
    }
}

fn attribute_string(element: AXUIElementRef, attribute_name: &str) -> Option<String> {
    let value = copy_attribute(element, attribute_name)?;
    cf_string(&value)
}

fn attribute_text(element: AXUIElementRef, attribute_name: &str) -> Option<String> {
    let value = copy_attribute(element, attribute_name)?;
    cf_string(&value).or_else(|| {
        if unsafe { CFGetTypeID(value.0) } == CFNumber::type_id() {
            // SAFETY: the type id was checked and wrapping under get retains it.
            let number = unsafe { CFNumber::wrap_under_get_rule(value.0.cast()) };
            number
                .to_i64()
                .map(|number| number.to_string())
                .or_else(|| number.to_f64().map(|number| number.to_string()))
        } else if unsafe { CFGetTypeID(value.0) } == CFBoolean::type_id() {
            // SAFETY: the type id was checked and wrapping under get retains it.
            let boolean = unsafe { CFBoolean::wrap_under_get_rule(value.0.cast()) };
            Some(boolean.into()).map(|value: bool| value.to_string())
        } else {
            None
        }
    })
}

fn cf_string(value: &OwnedCf) -> Option<String> {
    if unsafe { CFGetTypeID(value.0) } != CFString::type_id() {
        return None;
    }
    // SAFETY: the type id was checked and wrapping under get retains it.
    let value = unsafe { CFString::wrap_under_get_rule(value.0.cast()) };
    Some(value.to_string())
}

fn attribute_bool(element: AXUIElementRef, attribute_name: &str) -> Option<bool> {
    let value = copy_attribute(element, attribute_name)?;
    if unsafe { CFGetTypeID(value.0) } != CFBoolean::type_id() {
        return None;
    }
    // SAFETY: the type id was checked and wrapping under get retains it.
    let boolean = unsafe { CFBoolean::wrap_under_get_rule(value.0.cast()) };
    Some(boolean.into())
}

fn element_frame(element: AXUIElementRef) -> Frame {
    if let Some(value) = copy_attribute(element, "AXFrame")
        && let Some(rect) = ax_rect(&value)
    {
        return Frame {
            x: rect.origin.x,
            y: rect.origin.y,
            w: rect.size.width,
            h: rect.size.height,
        };
    }
    let point = copy_attribute(element, "AXPosition").and_then(|value| ax_point(&value));
    let size = copy_attribute(element, "AXSize").and_then(|value| ax_size(&value));
    match (point, size) {
        (Some(point), Some(size)) => Frame {
            x: point.x,
            y: point.y,
            w: size.width,
            h: size.height,
        },
        _ => Frame::default(),
    }
}

fn ax_point(value: &OwnedCf) -> Option<CGPoint> {
    if unsafe { CFGetTypeID(value.0) } != unsafe { AXValueGetTypeID() }
        || unsafe { AXValueGetType(value.0) } != AX_VALUE_CGPOINT
    {
        return None;
    }
    let mut point = CGPoint::new(0.0, 0.0);
    unsafe {
        AXValueGetValue(
            value.0,
            AX_VALUE_CGPOINT,
            (&mut point as *mut CGPoint).cast(),
        )
    }
    .then_some(point)
}

fn ax_size(value: &OwnedCf) -> Option<CGSize> {
    if unsafe { CFGetTypeID(value.0) } != unsafe { AXValueGetTypeID() }
        || unsafe { AXValueGetType(value.0) } != AX_VALUE_CGSIZE
    {
        return None;
    }
    let mut size = CGSize::new(0.0, 0.0);
    unsafe { AXValueGetValue(value.0, AX_VALUE_CGSIZE, (&mut size as *mut CGSize).cast()) }
        .then_some(size)
}

fn ax_rect(value: &OwnedCf) -> Option<CGRect> {
    if unsafe { CFGetTypeID(value.0) } != unsafe { AXValueGetTypeID() }
        || unsafe { AXValueGetType(value.0) } != AX_VALUE_CGRECT
    {
        return None;
    }
    let mut rect = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
    unsafe { AXValueGetValue(value.0, AX_VALUE_CGRECT, (&mut rect as *mut CGRect).cast()) }
        .then_some(rect)
}

fn action_names(element: AXUIElementRef) -> Vec<String> {
    let mut array: CFArrayRef = ptr::null();
    let code = unsafe { AXUIElementCopyActionNames(element, &mut array) };
    if code != AX_SUCCESS || array.is_null() {
        return Vec::new();
    }
    // SAFETY: CopyActionNames returns a create-rule CFArray.
    let Some(_owner) = (unsafe { OwnedCf::from_create(array as CFTypeRef) }) else {
        return Vec::new();
    };
    let count = unsafe { CFArrayGetCount(array) }.max(0) as usize;
    let mut actions = Vec::with_capacity(count);
    for index in 0..count {
        let value = unsafe { CFArrayGetValueAtIndex(array, index as isize) } as CFTypeRef;
        if value.is_null() || unsafe { CFGetTypeID(value) } != CFString::type_id() {
            continue;
        }
        // SAFETY: type checked; array owns the borrowed string for this scope.
        let value = unsafe { CFString::wrap_under_get_rule(value.cast()) };
        actions.push(canonical_role(&value.to_string()));
    }
    actions.sort();
    actions.dedup();
    actions
}

fn ax_result(operation: &'static str, code: AXError) -> Result<(), AxFailure> {
    if code == AX_SUCCESS {
        Ok(())
    } else {
        Err(AxFailure { operation, code })
    }
}

fn ax_error_text(code: AXError) -> String {
    format!("AX error {code} ({})", ax_error_name(code))
}

fn ax_error_name(code: AXError) -> &'static str {
    match code {
        0 => "success",
        -25200 => "failure",
        -25201 => "illegal argument",
        -25202 => "invalid UI element",
        -25203 => "invalid observer",
        -25204 => "cannot complete",
        -25205 => "attribute unsupported",
        -25206 => "action unsupported",
        -25207 => "notification unsupported",
        -25208 => "not implemented",
        -25209 => "notification already registered",
        -25210 => "notification not registered",
        -25211 => "API disabled",
        -25212 => "no value",
        -25213 => "parameterized attribute unsupported",
        -25214 => "not enough precision",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The system hands out a fresh object for every element reference, and
    /// reuses a released one's address: a walk that told nodes apart by
    /// address dropped the first child of a group whose address a freed
    /// sibling's child had used.
    #[test]
    fn visited_elements_are_told_apart_by_identity_not_address() {
        let element = |pid: i32| {
            // SAFETY: AXUIElementCreateApplication follows the create rule and
            // needs no permission to make a reference.
            unsafe { OwnedCf::from_create(AXUIElementCreateApplication(pid) as CFTypeRef) }
                .expect("an application element")
        };
        let mut visited = VisitedElements::default();
        let own = std::process::id() as i32;
        assert!(visited.insert(element(own).as_ax()));
        assert!(
            !visited.insert(element(own).as_ax()),
            "a second reference to the same element is the same node"
        );
        for pid in 1..=256 {
            let other = element(pid);
            assert!(visited.insert(other.as_ax()), "pid {pid} is a new element");
        }
    }
}
