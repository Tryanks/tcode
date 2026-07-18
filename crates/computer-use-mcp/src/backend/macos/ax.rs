use std::collections::HashSet;
use std::ffi::c_void;
use std::fmt;
use std::ptr;

use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation::base::{CFGetTypeID, CFRelease, CFRetain, CFTypeID, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use super::super::{BackendError, BackendErrorCode, RootInfo, RootKind};
use crate::outline::{Frame, UiNode, canonical_role};

type AXUIElementRef = CFTypeRef;
type AXValueRef = CFTypeRef;
type AXError = i32;
type AXValueType = u32;

const AX_SUCCESS: AXError = 0;
const AX_VALUE_CGPOINT: AXValueType = 1;
const AX_VALUE_CGSIZE: AXValueType = 2;
const AX_VALUE_CGRECT: AXValueType = 3;
const MAX_DEPTH: usize = 18;
const MAX_NODES: usize = 3_000;
const MAX_CHILDREN_PER_NODE: usize = 500;

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

    pub(super) fn text(&self) -> String {
        element_text(self.element.as_ax())
    }
}

pub(super) fn application_identifier(pid: u32) -> String {
    if let Some(identifier) = running_application_identifier(pid) {
        return identifier;
    }
    let Some(application) = create_application(pid) else {
        return String::new();
    };
    attribute_string(application.as_ax(), "AXIdentifier").unwrap_or_default()
}

fn running_application_identifier(pid: u32) -> Option<String> {
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

pub(super) fn observe_tree(root: &RootInfo) -> Result<UiNode, BackendError> {
    let (_application, window) = locate_window(root)?;
    let mut context = WalkContext {
        count: 0,
        visited: HashSet::new(),
        root_frame: root.frame,
    };
    let mut tree = walk_element(window.as_ax(), 0, &mut context).ok_or_else(|| {
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
        let children = copy_children(element.as_ax(), index.saturating_add(1));
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

pub(super) fn read_target_text(root: &RootInfo, path: &[usize]) -> Result<String, BackendError> {
    Ok(locate_target(root, path, None, None)?.text())
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
    let windows = copy_attribute_elements(application.as_ax(), "AXWindows", 200);
    let window = windows
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
        })?;
    Ok((application, window))
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
    visited: HashSet<usize>,
    root_frame: Frame,
}

fn walk_element(
    element: AXUIElementRef,
    depth: usize,
    context: &mut WalkContext,
) -> Option<UiNode> {
    if context.count >= MAX_NODES || !context.visited.insert(element as usize) {
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
    let mut node = UiNode {
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
    };

    if depth < MAX_DEPTH && context.count < MAX_NODES {
        let remaining = MAX_NODES - context.count;
        for child in copy_children(element, remaining.min(MAX_CHILDREN_PER_NODE)) {
            let Some(child) = walk_element(child.as_ax(), depth + 1, context) else {
                continue;
            };
            let invisible_leaf = child.children.is_empty()
                && !child.is_interactive()
                && child.title.is_empty()
                && child.value.is_empty()
                && (!child.frame.has_area()
                    || (context.root_frame.has_area()
                        && !child.frame.intersects(context.root_frame)));
            if !invisible_leaf {
                node.children.push(child);
            }
            if context.count >= MAX_NODES {
                break;
            }
        }
    }
    Some(node)
}

fn copy_children(element: AXUIElementRef, maximum: usize) -> Vec<OwnedCf> {
    copy_attribute_elements(element, "AXChildren", maximum)
}

fn copy_attribute_elements(
    element: AXUIElementRef,
    attribute_name: &str,
    maximum: usize,
) -> Vec<OwnedCf> {
    if maximum == 0 {
        return Vec::new();
    }
    let attribute = CFString::new(attribute_name);
    let mut count = 0_isize;
    let count_code = unsafe {
        AXUIElementGetAttributeValueCount(element, attribute.as_concrete_TypeRef(), &mut count)
    };
    if count_code != AX_SUCCESS || count <= 0 {
        return Vec::new();
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
    if code != AX_SUCCESS || array.is_null() {
        return Vec::new();
    }
    // SAFETY: CopyAttributeValues returns a create-rule CFArray.
    let Some(array_owner) = (unsafe { OwnedCf::from_create(array as CFTypeRef) }) else {
        return Vec::new();
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
    values
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

fn element_text(element: AXUIElementRef) -> String {
    let mut values = Vec::new();
    for attribute in [
        "AXSelectedText",
        "AXValue",
        "AXTitle",
        "AXDescription",
        "AXHelp",
    ] {
        if let Some(value) = attribute_text(element, attribute)
            && !value.is_empty()
            && !values.contains(&value)
        {
            values.push(value);
        }
    }
    values.join("\n")
}

fn ax_result(operation: &'static str, code: AXError) -> Result<(), AxFailure> {
    if code == AX_SUCCESS {
        Ok(())
    } else {
        Err(AxFailure { operation, code })
    }
}

fn ax_error_name(code: AXError) -> &'static str {
    match code {
        0 => "success",
        -25201 => "failure",
        -25202 => "illegal argument",
        -25203 => "invalid UI element",
        -25204 => "invalid observer",
        -25205 => "cannot complete",
        -25206 => "attribute unsupported",
        -25207 => "action unsupported",
        -25208 => "notification unsupported",
        -25209 => "not implemented",
        -25210 => "notification already registered",
        -25211 => "notification not registered",
        -25212 => "API disabled",
        -25213 => "no value",
        -25214 => "parameterized attribute unsupported",
        -25215 => "not enough precision",
        _ => "unknown",
    }
}
