use std::collections::HashSet;
use std::ffi::c_void;
use std::fmt;

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, RECT, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded,
    ExpandCollapseState_PartiallyExpanded, IUIAutomation, IUIAutomationElement,
    IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern,
    IUIAutomationLegacyIAccessiblePattern, IUIAutomationRangeValuePattern,
    IUIAutomationScrollItemPattern, IUIAutomationScrollPattern, IUIAutomationSelectionItemPattern,
    IUIAutomationTogglePattern, IUIAutomationTreeWalker, IUIAutomationValuePattern,
    IUIAutomationWindowPattern, ScrollAmount_LargeDecrement, ScrollAmount_LargeIncrement,
    ScrollAmount_NoAmount, ScrollAmount_SmallDecrement, ScrollAmount_SmallIncrement,
    ToggleState_Indeterminate, ToggleState_Off, ToggleState_On, UIA_ExpandCollapsePatternId,
    UIA_InvokePatternId, UIA_LegacyIAccessiblePatternId, UIA_RangeValuePatternId,
    UIA_ScrollItemPatternId, UIA_ScrollPatternId, UIA_SelectionItemPatternId, UIA_TogglePatternId,
    UIA_ValuePatternId, UIA_WindowPatternId,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindow, IsWindowVisible,
};
use windows::core::{BOOL, BSTR, Interface, PWSTR};

use super::super::{
    BackendError, BackendErrorCode, RootFilters, RootInfo, RootKind, matches_root_filters,
};
use super::pure::{
    PatternSupport, actions_for_patterns, frame_from_uia_rect, role_for_control_type,
    root_kind_for_control_type,
};
use crate::outline::{Frame, UiNode, canonical_role};

const MAX_DEPTH: usize = 18;
const MAX_NODES: usize = 3_000;
const MAX_CHILDREN_PER_NODE: usize = 500;
const PROCESS_PATH_CAPACITY: usize = 32_768;

pub(super) fn list_roots(filters: &RootFilters) -> Result<Vec<RootInfo>, BackendError> {
    let automation = Automation::new(
        BackendErrorCode::ObservationFailed,
        "initializing UIAutomation for root enumeration",
    )?;
    let mut roots = Vec::new();
    for hwnd in enumerate_top_level_windows(BackendErrorCode::ObservationFailed)? {
        let Ok(element) = (unsafe { automation.client.ElementFromHandle(hwnd) }) else {
            continue;
        };
        let control_type = unsafe { element.CurrentControlType() }
            .map(|value| value.0)
            .unwrap_or_default();
        let localized_control_type = element_string(&element, ElementString::LocalizedControlType);
        let class_name = element_string(&element, ElementString::ClassName);
        let Some(mut kind) =
            root_kind_for_control_type(control_type, &localized_control_type, &class_name)
        else {
            continue;
        };
        if kind == RootKind::Window && element_is_modal_window(&element) {
            kind = RootKind::Dialog;
        }
        let pid = unsafe { element.CurrentProcessId() }
            .ok()
            .and_then(|pid| u32::try_from(pid).ok())
            .or_else(|| window_pid(hwnd))
            .unwrap_or_default();
        if pid == 0 {
            continue;
        }
        let frame = element_frame(&element);
        if !frame.has_area() {
            continue;
        }
        let title = nonempty(element_string(&element, ElementString::Name))
            .unwrap_or_else(|| window_title(hwnd));
        let executable_name = process_executable_name(pid).unwrap_or_default();
        let app_name = app_display_name(&executable_name, &class_name);
        let root = RootInfo {
            ref_id: String::new(),
            app_name,
            // Windows has no bundle identifier. The executable filename is
            // stable enough to support the same filter field.
            bundle_id: executable_name,
            pid,
            title,
            kind,
            window_id: hwnd_id(hwnd),
            frame,
        };
        if root.window_id != 0 && matches_root_filters(&root, filters) {
            roots.push(root);
        }
    }
    Ok(roots)
}

pub(super) fn observe_tree(root: &RootInfo) -> Result<UiNode, BackendError> {
    let hwnd = locate_hwnd(root)?;
    let automation = Automation::new(
        BackendErrorCode::ObservationFailed,
        "initializing UIAutomation for observation",
    )?;
    let element = unsafe { automation.client.ElementFromHandle(hwnd) }.map_err(|error| {
        backend_error(
            BackendErrorCode::RootNotFound,
            format!("UIAutomation could not open root {}: {error}", root.ref_id),
        )
    })?;
    let mut context = WalkContext {
        count: 0,
        visited: HashSet::new(),
        root_frame: root.frame,
        walker: &automation.walker,
    };
    let mut tree = walk_element(&element, 0, &mut context).ok_or_else(|| {
        backend_error(
            BackendErrorCode::ObservationFailed,
            format!("the UIAutomation tree for {} was empty", root.title),
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
    let hwnd = locate_hwnd(root)?;
    let automation = Automation::new(
        BackendErrorCode::OperationFailed,
        "initializing UIAutomation for an action",
    )?;
    let mut element = unsafe { automation.client.ElementFromHandle(hwnd) }.map_err(|error| {
        backend_error(
            BackendErrorCode::RootNotFound,
            format!(
                "UIAutomation could not reopen root {}: {error}",
                root.ref_id
            ),
        )
    })?;
    for &index in path {
        let children = included_children(&automation.walker, &element, root.frame);
        let Some(child) = children.into_iter().nth(index) else {
            return Err(backend_error(
                BackendErrorCode::OperationFailed,
                "the target's UIAutomation path moved; call observe_ui again",
            ));
        };
        element = child;
    }

    if let Some(expected_role) = expected_role {
        let actual = element_role(&element);
        if canonical_role(&actual) != canonical_role(expected_role) {
            return Err(backend_error(
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
        let actual = element_string(&element, ElementString::Name);
        if !actual.is_empty() && actual != expected_title {
            return Err(backend_error(
                BackendErrorCode::OperationFailed,
                "target title changed; call observe_ui again",
            ));
        }
    }
    Ok(Target {
        element,
        _automation: automation,
    })
}

pub(super) fn locate_hwnd(root: &RootInfo) -> Result<HWND, BackendError> {
    let direct = HWND(root.window_id as usize as *mut c_void);
    if window_matches_pid(direct, root.pid) {
        return Ok(direct);
    }

    enumerate_top_level_windows(BackendErrorCode::RootNotFound)?
        .into_iter()
        .filter(|hwnd| window_matches_pid(*hwnd, root.pid))
        .max_by(|left, right| {
            window_match_score(*left, root).total_cmp(&window_match_score(*right, root))
        })
        .ok_or_else(|| {
            backend_error(
                BackendErrorCode::RootNotFound,
                format!("no top-level HWND matched root {}", root.ref_id),
            )
        })
}

pub(super) struct Target {
    element: IUIAutomationElement,
    _automation: Automation,
}

impl Target {
    pub(super) fn press(&self) -> Result<&'static str, UiaFailure> {
        let mut failures = Vec::new();
        if let Ok(pattern) = unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        } {
            match unsafe { pattern.Invoke() } {
                Ok(()) => return Ok("UIA InvokePattern completed"),
                Err(error) => failures.push(format!("InvokePattern failed: {error}")),
            }
        }
        if let Ok(pattern) = unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
        } {
            match unsafe { pattern.Toggle() } {
                Ok(()) => return Ok("UIA TogglePattern completed"),
                Err(error) => failures.push(format!("TogglePattern failed: {error}")),
            }
        }
        if let Ok(pattern) = unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                    UIA_ExpandCollapsePatternId,
                )
        } {
            let result = match unsafe { pattern.CurrentExpandCollapseState() } {
                Ok(state)
                    if state == ExpandCollapseState_Expanded
                        || state == ExpandCollapseState_PartiallyExpanded =>
                {
                    Some(
                        unsafe { pattern.Collapse() }
                            .map(|()| "UIA ExpandCollapsePattern collapsed"),
                    )
                }
                Ok(state) if state == ExpandCollapseState_Collapsed => {
                    Some(unsafe { pattern.Expand() }.map(|()| "UIA ExpandCollapsePattern expanded"))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            };
            if let Some(result) = result {
                match result {
                    Ok(message) => return Ok(message),
                    Err(error) => failures.push(format!("ExpandCollapsePattern failed: {error}")),
                }
            }
        }
        if let Ok(pattern) = unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
        } {
            match unsafe { pattern.Select() } {
                Ok(()) => return Ok("UIA SelectionItemPattern selected the target"),
                Err(error) => failures.push(format!("SelectionItemPattern failed: {error}")),
            }
        }
        if let Ok(pattern) = unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                    UIA_LegacyIAccessiblePatternId,
                )
        } {
            match unsafe { pattern.DoDefaultAction() } {
                Ok(()) => return Ok("UIA legacy default action completed"),
                Err(error) => failures.push(format!("legacy default action failed: {error}")),
            }
        }
        Err(UiaFailure::from_attempts("press", failures))
    }

    pub(super) fn set_text(&self, text: &str) -> Result<&'static str, UiaFailure> {
        let mut failures = Vec::new();
        if let Ok(pattern) = unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
        } {
            let value = BSTR::from(text);
            match unsafe { pattern.SetValue(&value) } {
                Ok(()) => return Ok("UIA ValuePattern value was set"),
                Err(error) => failures.push(format!("ValuePattern failed: {error}")),
            }
        }
        if let Ok(pattern) = unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
        } {
            match text.parse::<f64>() {
                Ok(value) => match unsafe { pattern.SetValue(value) } {
                    Ok(()) => return Ok("UIA RangeValuePattern value was set"),
                    Err(error) => failures.push(format!("RangeValuePattern failed: {error}")),
                },
                Err(error) => {
                    failures.push(format!("RangeValuePattern requires a number: {error}"))
                }
            }
        }
        if let Ok(pattern) = unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                    UIA_LegacyIAccessiblePatternId,
                )
        } {
            let value = BSTR::from(text);
            match unsafe { pattern.SetValue(&value) } {
                Ok(()) => return Ok("UIA legacy value was set"),
                Err(error) => failures.push(format!("legacy value failed: {error}")),
            }
        }
        Err(UiaFailure::from_attempts("set_text", failures))
    }

    pub(super) fn focus(&self) -> Result<(), UiaFailure> {
        unsafe { self.element.SetFocus() }
            .map_err(|error| UiaFailure(format!("UIA SetFocus failed: {error}")))
    }

    pub(super) fn scroll(&self, x: f64, y: f64) -> Result<&'static str, UiaFailure> {
        let pattern = unsafe {
            self.element
                .GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
        }
        .map_err(|error| UiaFailure(format!("UIA ScrollPattern is unavailable: {error}")))?;
        let horizontal = scroll_amount(x, false);
        let vertical = scroll_amount(y, true);
        if horizontal == ScrollAmount_NoAmount && vertical == ScrollAmount_NoAmount {
            return Err(UiaFailure("UIA scroll deltas were both zero".into()));
        }
        unsafe { pattern.Scroll(horizontal, vertical) }
            .map(|()| "UIA ScrollPattern completed")
            .map_err(|error| UiaFailure(format!("UIA ScrollPattern failed: {error}")))
    }

    pub(super) fn frame(&self) -> Frame {
        element_frame(&self.element)
    }
}

#[derive(Debug)]
pub(super) struct UiaFailure(String);

impl UiaFailure {
    fn from_attempts(operation: &str, failures: Vec<String>) -> Self {
        if failures.is_empty() {
            Self(format!(
                "UIAutomation target exposes no pattern for {operation}"
            ))
        } else {
            Self(failures.join("; "))
        }
    }
}

impl fmt::Display for UiaFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

struct Automation {
    client: IUIAutomation,
    walker: IUIAutomationTreeWalker,
    _apartment: ComApartment,
}

impl Automation {
    fn new(code: BackendErrorCode, operation: &str) -> Result<Self, BackendError> {
        let apartment = ComApartment::initialize(code, operation)?;
        let client: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.map_err(
                |error| {
                    backend_error(
                        code,
                        format!("{operation}: CoCreateInstance(CUIAutomation) failed: {error}"),
                    )
                },
            )?;
        let walker = unsafe { client.ControlViewWalker() }.map_err(|error| {
            backend_error(
                code,
                format!("{operation}: ControlViewWalker failed: {error}"),
            )
        })?;
        Ok(Self {
            client,
            walker,
            _apartment: apartment,
        })
    }
}

struct ComApartment {
    should_uninitialize: bool,
}

impl ComApartment {
    fn initialize(code: BackendErrorCode, operation: &str) -> Result<Self, BackendError> {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result == RPC_E_CHANGED_MODE {
            // The runtime initialized this thread with another apartment model.
            // COM is still usable; do not balance an initialization we did not make.
            return Ok(Self {
                should_uninitialize: false,
            });
        }
        result.ok().map_err(|error| {
            backend_error(code, format!("{operation}: CoInitializeEx failed: {error}"))
        })?;
        Ok(Self {
            should_uninitialize: true,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.should_uninitialize {
            unsafe { CoUninitialize() };
        }
    }
}

struct WalkContext<'a> {
    count: usize,
    visited: HashSet<usize>,
    root_frame: Frame,
    walker: &'a IUIAutomationTreeWalker,
}

fn walk_element(
    element: &IUIAutomationElement,
    depth: usize,
    context: &mut WalkContext<'_>,
) -> Option<UiNode> {
    let identity = Interface::as_raw(element) as usize;
    if context.count >= MAX_NODES || !context.visited.insert(identity) {
        return None;
    }
    context.count += 1;
    let title = element_string(element, ElementString::Name);
    let value = element_value(element);
    let description = [ElementString::HelpText, ElementString::ItemStatus]
        .into_iter()
        .map(|property| element_string(element, property))
        .find(|description| !description.is_empty() && description != &title)
        .unwrap_or_default();
    let frame = element_frame(element);
    let actions = element_actions(element);
    let enabled = unsafe { element.CurrentIsEnabled() }
        .map(|value| value.as_bool())
        .unwrap_or(true);
    let focused = unsafe { element.CurrentHasKeyboardFocus() }
        .map(|value| value.as_bool())
        .unwrap_or(false);
    let mut node = UiNode {
        ref_id: String::new(),
        role: element_role(element),
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
        for child in raw_children(
            context.walker,
            element,
            remaining.min(MAX_CHILDREN_PER_NODE),
        ) {
            let Some(child) = walk_element(&child, depth + 1, context) else {
                continue;
            };
            if !is_invisible_leaf(&child, context.root_frame) {
                node.children.push(child);
            }
            if context.count >= MAX_NODES {
                break;
            }
        }
    }
    Some(node)
}

fn raw_children(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    maximum: usize,
) -> Vec<IUIAutomationElement> {
    if maximum == 0 {
        return Vec::new();
    }
    let Ok(mut child) = (unsafe { walker.GetFirstChildElement(element) }) else {
        return Vec::new();
    };
    let mut children = Vec::with_capacity(maximum.min(32));
    loop {
        children.push(child.clone());
        if children.len() >= maximum {
            break;
        }
        let Ok(sibling) = (unsafe { walker.GetNextSiblingElement(&child) }) else {
            break;
        };
        child = sibling;
    }
    children
}

fn included_children(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    root_frame: Frame,
) -> Vec<IUIAutomationElement> {
    raw_children(walker, element, MAX_CHILDREN_PER_NODE)
        .into_iter()
        .filter(|child| {
            let has_children = unsafe { walker.GetFirstChildElement(child) }.is_ok();
            if has_children {
                return true;
            }
            let node = UiNode {
                role: element_role(child),
                title: element_string(child, ElementString::Name),
                value: element_value(child),
                description: element_string(child, ElementString::HelpText),
                frame: element_frame(child),
                actions: element_actions(child),
                enabled: true,
                ..UiNode::default()
            };
            !is_invisible_leaf(&node, root_frame)
        })
        .collect()
}

fn is_invisible_leaf(node: &UiNode, root_frame: Frame) -> bool {
    node.children.is_empty()
        && !node.is_interactive()
        && node.title.is_empty()
        && node.value.is_empty()
        && (!node.frame.has_area() || (root_frame.has_area() && !node.frame.intersects(root_frame)))
}

fn element_role(element: &IUIAutomationElement) -> String {
    unsafe { element.CurrentControlType() }
        .map(|value| role_for_control_type(value.0).to_string())
        .unwrap_or_else(|_| "unknown".into())
}

fn element_frame(element: &IUIAutomationElement) -> Frame {
    unsafe { element.CurrentBoundingRectangle() }
        .map(|rect| frame_from_uia_rect(rect.left, rect.top, rect.right, rect.bottom))
        .unwrap_or_default()
}

fn element_value(element: &IUIAutomationElement) -> String {
    if let Ok(pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
        && let Ok(value) = unsafe { pattern.CurrentValue() }
    {
        return value.to_string();
    }
    if let Ok(pattern) = unsafe {
        element.GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
    } && let Ok(value) = unsafe { pattern.CurrentValue() }
    {
        return value.to_string();
    }
    if let Ok(pattern) =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) }
        && let Ok(value) = unsafe { pattern.CurrentToggleState() }
    {
        return if value == ToggleState_On {
            "true".into()
        } else if value == ToggleState_Off {
            "false".into()
        } else if value == ToggleState_Indeterminate {
            "mixed".into()
        } else {
            String::new()
        };
    }
    if let Ok(pattern) = unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId)
    } && let Ok(value) = unsafe { pattern.CurrentExpandCollapseState() }
    {
        return if value == ExpandCollapseState_Expanded {
            "expanded".into()
        } else if value == ExpandCollapseState_Collapsed {
            "collapsed".into()
        } else if value == ExpandCollapseState_PartiallyExpanded {
            "partially_expanded".into()
        } else {
            String::new()
        };
    }
    if let Ok(pattern) = unsafe {
        element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
    } && let Ok(value) = unsafe { pattern.CurrentIsSelected() }
    {
        return value.as_bool().to_string();
    }
    if let Ok(pattern) = unsafe {
        element.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
            UIA_LegacyIAccessiblePatternId,
        )
    } && let Ok(value) = unsafe { pattern.CurrentValue() }
    {
        return value.to_string();
    }
    String::new()
}

fn element_actions(element: &IUIAutomationElement) -> Vec<String> {
    let value_writable =
        unsafe { element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) }
            .ok()
            .is_some_and(|pattern| {
                unsafe { pattern.CurrentIsReadOnly() }
                    .map(|read_only| !read_only.as_bool())
                    .unwrap_or(true)
            });
    let range_value_writable = unsafe {
        element.GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
    }
    .ok()
    .is_some_and(|pattern| {
        unsafe { pattern.CurrentIsReadOnly() }
            .map(|read_only| !read_only.as_bool())
            .unwrap_or(true)
    });
    actions_for_patterns(PatternSupport {
        invoke: unsafe {
            element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        }
        .is_ok(),
        value_writable,
        range_value_writable,
        toggle: unsafe {
            element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
        }
        .is_ok(),
        expand_collapse: unsafe {
            element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                UIA_ExpandCollapsePatternId,
            )
        }
        .is_ok(),
        scroll: unsafe {
            element.GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
        }
        .is_ok(),
        scroll_item: unsafe {
            element.GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId)
        }
        .is_ok(),
        selection_item: unsafe {
            element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                UIA_SelectionItemPatternId,
            )
        }
        .is_ok(),
        legacy_default: unsafe {
            element.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
                UIA_LegacyIAccessiblePatternId,
            )
        }
        .ok()
        .and_then(|pattern| unsafe { pattern.CurrentDefaultAction() }.ok())
        .is_some_and(|action| !action.to_string().trim().is_empty()),
    })
}

fn element_is_modal_window(element: &IUIAutomationElement) -> bool {
    unsafe { element.GetCurrentPatternAs::<IUIAutomationWindowPattern>(UIA_WindowPatternId) }
        .ok()
        .and_then(|pattern| unsafe { pattern.CurrentIsModal() }.ok())
        .is_some_and(|modal| modal.as_bool())
}

#[derive(Clone, Copy)]
enum ElementString {
    Name,
    LocalizedControlType,
    ClassName,
    HelpText,
    ItemStatus,
}

fn element_string(element: &IUIAutomationElement, property: ElementString) -> String {
    let result = unsafe {
        match property {
            ElementString::Name => element.CurrentName(),
            ElementString::LocalizedControlType => element.CurrentLocalizedControlType(),
            ElementString::ClassName => element.CurrentClassName(),
            ElementString::HelpText => element.CurrentHelpText(),
            ElementString::ItemStatus => element.CurrentItemStatus(),
        }
    };
    result.map(|value| value.to_string()).unwrap_or_default()
}

fn scroll_amount(value: f64, invert: bool) -> windows::Win32::UI::Accessibility::ScrollAmount {
    let value = if invert { -value } else { value };
    if value == 0.0 || !value.is_finite() {
        ScrollAmount_NoAmount
    } else if value >= 120.0 {
        ScrollAmount_LargeIncrement
    } else if value > 0.0 {
        ScrollAmount_SmallIncrement
    } else if value <= -120.0 {
        ScrollAmount_LargeDecrement
    } else {
        ScrollAmount_SmallDecrement
    }
}

fn enumerate_top_level_windows(code: BackendErrorCode) -> Result<Vec<HWND>, BackendError> {
    unsafe extern "system" fn callback(hwnd: HWND, parameter: LPARAM) -> BOOL {
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let windows = unsafe { &mut *(parameter.0 as *mut Vec<HWND>) };
            windows.push(hwnd);
        }
        BOOL::from(true)
    }

    let mut windows = Vec::new();
    unsafe {
        EnumWindows(
            Some(callback),
            LPARAM((&mut windows as *mut Vec<HWND>) as isize),
        )
    }
    .map_err(|error| backend_error(code, format!("EnumWindows failed: {error}")))?;
    Ok(windows)
}

fn window_pid(hwnd: HWND) -> Option<u32> {
    let mut pid = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

fn window_matches_pid(hwnd: HWND, pid: u32) -> bool {
    !hwnd.is_invalid() && unsafe { IsWindow(Some(hwnd)) }.as_bool() && window_pid(hwnd) == Some(pid)
}

fn hwnd_id(hwnd: HWND) -> u32 {
    hwnd.0 as usize as u32
}

fn window_title(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    let Ok(capacity) = usize::try_from(length.saturating_add(1)) else {
        return String::new();
    };
    if capacity <= 1 {
        return String::new();
    }
    let mut buffer = vec![0_u16; capacity];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    let Ok(copied) = usize::try_from(copied) else {
        return String::new();
    };
    String::from_utf16_lossy(&buffer[..copied.min(buffer.len())])
}

fn window_frame(hwnd: HWND) -> Frame {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }
        .map(|()| frame_from_uia_rect(rect.left, rect.top, rect.right, rect.bottom))
        .unwrap_or_default()
}

fn window_match_score(hwnd: HWND, root: &RootInfo) -> f64 {
    let title = window_title(hwnd);
    let frame = window_frame(hwnd);
    let title_score = if !root.title.is_empty() && title == root.title {
        1_000_000.0
    } else if !root.title.is_empty()
        && title
            .to_ascii_lowercase()
            .contains(&root.title.to_ascii_lowercase())
    {
        100_000.0
    } else {
        0.0
    };
    let frame_distance = (frame.x - root.frame.x).abs()
        + (frame.y - root.frame.y).abs()
        + (frame.w - root.frame.w).abs()
        + (frame.h - root.frame.h).abs();
    title_score - frame_distance
}

fn process_executable_name(pid: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let _handle = OwnedProcessHandle(handle);
    let mut buffer = vec![0_u16; PROCESS_PATH_CAPACITY];
    let mut length = u32::try_from(buffer.len()).ok()?;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    let length = usize::try_from(length).ok()?.min(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..length]);
    nonempty(
        path.rsplit(['\\', '/'])
            .next()
            .unwrap_or_default()
            .to_string(),
    )
}

struct OwnedProcessHandle(HANDLE);

impl Drop for OwnedProcessHandle {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

fn app_display_name(executable_name: &str, class_name: &str) -> String {
    if !executable_name.is_empty() {
        if executable_name.to_ascii_lowercase().ends_with(".exe") {
            return executable_name[..executable_name.len() - 4].to_string();
        }
        return executable_name.to_string();
    }
    class_name.to_string()
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn backend_error(code: BackendErrorCode, message: impl Into<String>) -> BackendError {
    BackendError::new(code, message)
}
