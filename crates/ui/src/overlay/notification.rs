use std::{
    any::TypeId,
    collections::HashMap,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use gpui::{
    AnyElement, App, AppContext as _, Context, DismissEvent, ElementId, Entity, EventEmitter,
    FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{
    Toast as BaseToast, ToastManager, ToastMotion, ToastOptions, ToastStack, ToastStackState,
    ToastTransitionStatus,
};

use crate::{
    icon::{Icon, IconName},
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    typography::TextRole as _,
    widgets::{Button, ButtonVariants as _},
};
use gpui_base::StyledExt as _;

/// How long a surface drawn here takes to appear, and to leave.
const APPEAR: std::time::Duration = std::time::Duration::from_millis(200);

const DEFAULT_WIDTH: gpui::Pixels = px(356.);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default)]
pub enum NotificationType {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

impl NotificationType {
    fn icon(self, cx: &App) -> Icon {
        match self {
            Self::Info => Icon::new(IconName::Info).text_color(cx.theme().info),
            Self::Success => Icon::new(IconName::CircleCheck).text_color(cx.theme().success),
            Self::Warning => Icon::new(IconName::TriangleAlert).text_color(cx.theme().warning),
            Self::Error => Icon::new(IconName::CircleX).text_color(cx.theme().danger),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum NotificationId {
    Type(TypeId),
    Key(TypeId, ElementId),
}

impl From<(TypeId, ElementId)> for NotificationId {
    fn from((kind, key): (TypeId, ElementId)) -> Self {
        Self::Key(kind, key)
    }
}

struct AnonymousNotification;
struct DismissRequest;

type ContentBuilder =
    Rc<dyn Fn(&mut Notification, &mut Window, &mut Context<Notification>) -> AnyElement>;
type ActionBuilder =
    Rc<dyn Fn(&mut Notification, &mut Window, &mut Context<Notification>) -> Button>;

pub struct Notification {
    id: NotificationId,
    type_: Option<NotificationType>,
    message: Option<SharedString>,
    title: Option<SharedString>,
    compact_message: Option<SharedString>,
    icon: Option<Icon>,
    autohide: bool,
    content: Option<ContentBuilder>,
    action: Option<ActionBuilder>,
    transition_status: ToastTransitionStatus,
    /// Its coming and going, kept here: the window draws a toast under
    /// another parent as it turns compact or wide (the pill, or a card in
    /// the stack), where a clock kept by its element path would bring it in
    /// again.
    motion: crate::motion::owned::Owned,
}

impl Notification {
    pub fn new() -> Self {
        Self {
            id: NotificationId::Key(
                TypeId::of::<AnonymousNotification>(),
                ElementId::NamedInteger(
                    "notification".into(),
                    NEXT_ID.fetch_add(1, Ordering::Relaxed),
                ),
            ),
            type_: None,
            message: None,
            title: None,
            compact_message: None,
            icon: None,
            autohide: true,
            content: None,
            action: None,
            transition_status: ToastTransitionStatus::Starting,
            motion: Default::default(),
        }
    }
    pub fn info(message: impl Into<SharedString>) -> Self {
        Self::new()
            .message(message)
            .with_type(NotificationType::Info)
    }
    pub fn success(message: impl Into<SharedString>) -> Self {
        Self::new()
            .message(message)
            .with_type(NotificationType::Success)
    }
    pub fn warning(message: impl Into<SharedString>) -> Self {
        Self::new()
            .message(message)
            .with_type(NotificationType::Warning)
    }
    pub fn error(message: impl Into<SharedString>) -> Self {
        Self::new()
            .message(message)
            .with_type(NotificationType::Error)
    }
    pub fn message(mut self, message: impl Into<SharedString>) -> Self {
        self.message = Some(message.into());
        self
    }
    pub(crate) fn compact_message(mut self, message: SharedString) -> Self {
        self.compact_message = Some(message);
        self
    }

    pub fn title(mut self, title: impl Into<SharedString>) -> Self {
        self.title = Some(title.into());
        self
    }
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }
    pub fn with_type(mut self, type_: NotificationType) -> Self {
        self.type_ = Some(type_);
        self
    }
    pub fn autohide(mut self, value: bool) -> Self {
        self.autohide = value;
        self
    }
    pub fn id<T: Sized + 'static>(mut self) -> Self {
        self.id = NotificationId::Type(TypeId::of::<T>());
        self
    }
    pub fn id1<T: Sized + 'static>(mut self, key: impl Into<ElementId>) -> Self {
        self.id = NotificationId::Key(TypeId::of::<T>(), key.into());
        self
    }
    pub fn content(
        mut self,
        builder: impl Fn(&mut Self, &mut Window, &mut Context<Self>) -> AnyElement + 'static,
    ) -> Self {
        self.content = Some(Rc::new(builder));
        self
    }
    pub fn action(
        mut self,
        builder: impl Fn(&mut Self, &mut Window, &mut Context<Self>) -> Button + 'static,
    ) -> Self {
        self.action = Some(Rc::new(builder));
        self
    }
    fn is_error(&self) -> bool {
        matches!(self.type_, Some(NotificationType::Error))
    }

    pub(super) fn dialog_title(&self) -> Option<SharedString> {
        self.title.clone().or_else(|| self.message.clone())
    }

    pub(super) fn dialog_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let content = self.content.clone().map(|build| build(self, window, cx));
        let action = self.action.clone().map(|build| build(self, window, cx));
        div()
            .flex()
            .flex_col()
            .gap_3()
            .when(self.title.is_some(), |el| el.children(self.message.clone()))
            .children(content)
            .when_some(action, |el, action| {
                el.child(
                    div()
                        .id("notification-dialog-action")
                        .on_click(|_, window, cx| {
                            use super::OverlayExt as _;
                            window.close_dialog(cx);
                        })
                        .child(action),
                )
            })
            .into_any_element()
    }

    pub fn dismiss(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissRequest);
    }
}

impl Default for Notification {
    fn default() -> Self {
        Self::new()
    }
}
impl From<String> for Notification {
    fn from(value: String) -> Self {
        Self::new().message(value)
    }
}
impl From<SharedString> for Notification {
    fn from(value: SharedString) -> Self {
        Self::new().message(value)
    }
}
impl From<&str> for Notification {
    fn from(value: &str) -> Self {
        Self::new().message(value)
    }
}
impl EventEmitter<DismissRequest> for Notification {}
impl EventEmitter<DismissEvent> for Notification {}

impl Render for Notification {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let present = self.transition_status != ToastTransitionStatus::Ending;
        let progress = self.motion.appear("presence", present, APPEAR, window, cx);
        if !present && progress <= 0. {
            return div().into_any_element();
        }
        // gpui-base has no inert subtree: the cover takes the pointer from a
        // leaving toast, but a control already focused in it keeps focus.
        let cover = (!present).then(|| div().absolute().inset_0().occlude());
        let lift = crate::motion::lift(progress);
        if crate::window_seam::window_is_compact(window, cx) {
            let action = self
                .action
                .clone()
                .filter(|_| !self.is_error())
                .map(|build| build(self, window, cx).ghost());
            let icon = self.icon.clone().or_else(|| {
                self.type_.and_then(|kind| {
                    (!matches!(kind, NotificationType::Info)).then(|| kind.icon(cx))
                })
            });
            let spacing = &cx.theme().tokens.spacing;
            return div()
                .id("compact-toast")
                .debug_selector(|| "compact-toast".into())
                .role(gpui::Role::Alert)
                .occlude()
                .flex()
                .items_center()
                .gap(spacing.sm)
                .min_h(px(crate::material::TOUCH_TARGET))
                .max_w_full()
                .py(spacing.md)
                .px(spacing.lg)
                .text_body(true, cx)
                .rounded_full()
                .bg(cx.theme().popover)
                .shadow_lg()
                .text_color(cx.theme().foreground)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.dismiss(window, cx);
                    if this.is_error() {
                        let note = cx.entity();
                        // The dialog reads this entity after its click update completes.
                        window.defer(cx, move |window, cx| {
                            super::open_notification_dialog(note, window, cx);
                        });
                    }
                }))
                .children(icon)
                .child(
                    div().min_w_0().line_clamp(2).child(
                        self.compact_message
                            .clone()
                            .or_else(|| self.message.clone())
                            .or_else(|| self.title.clone())
                            .unwrap_or_default(),
                    ),
                )
                .children(action)
                .children(cover)
                .opacity(progress)
                .relative()
                .top(lift)
                .into_any_element();
        }
        let content = self
            .content
            .clone()
            .map(|builder| builder(self, window, cx));
        let action = self
            .action
            .clone()
            .map(|builder| builder(self, window, cx).small());
        let icon = self
            .icon
            .clone()
            .or_else(|| self.type_.map(|kind| kind.icon(cx)));
        let has_icon = icon.is_some();
        let spacing = &cx.theme().tokens.spacing;
        BaseToast::new("notification")
            .debug_selector(|| "wide-toast".into())
            .transition_status(self.transition_status)
            .flex()
            .items_start()
            .gap(spacing.md)
            .relative()
            .top(lift)
            .opacity(progress)
            .occlude()
            .w_full()
            .p(spacing.lg)
            .text_body(false, cx)
            .rounded(crate::material::radius_overlay(cx))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .shadow_lg()
            .when_some(icon, |el, icon| el.child(icon))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(spacing.xs)
                    .when_some(self.title.clone(), |el, title| {
                        el.child(div().font_semibold().child(title))
                    })
                    .when_some(self.message.clone(), |el, message| el.child(message))
                    .when_some(content, |el, content| el.child(content))
                    .when(!has_icon, |el| el),
            )
            .when_some(action, |el, action| el.child(action))
            .child(
                div().absolute().top_1().right_1().child(
                    Button::new("notification-close")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.dismiss(window, cx);
                        })),
                ),
            )
            .children(cover)
            .into_any_element()
    }
}

/// How long a toast takes to leave, and a compact one to enter.
const TOAST: std::time::Duration = std::time::Duration::from_millis(200);

/// A leaving toast stays mounted for its `appear` exit. A compact toast also
/// enters over [`TOAST`]; a wide one keeps the stack's own sonner timing.
fn toast_manager(compact: bool) -> ToastManager<NotificationId, Entity<Notification>> {
    let sonner = ToastMotion::sonner();
    ToastManager::new(ToastMotion {
        duration: if compact { TOAST } else { sonner.duration },
        exit_duration: TOAST,
        ..sonner
    })
}

pub(crate) struct NotificationList {
    manager: ToastManager<NotificationId, Entity<Notification>>,
    stack_state: ToastStackState,
    compact: bool,
    focus_handle: FocusHandle,
    subscriptions: HashMap<NotificationId, Subscription>,
    /// Whether the clock that ages the notifications runs. It runs only
    /// while one of them changes with time, so a window with nothing to show
    /// never wakes for it.
    ticking: bool,
}

impl NotificationList {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            manager: toast_manager(false),
            stack_state: ToastStackState::default(),
            compact: false,
            focus_handle: cx.focus_handle().tab_stop(true),
            subscriptions: HashMap::new(),
            ticking: false,
        }
    }

    /// Starts the clock unless it runs or nothing needs it; it stops itself
    /// once no notification changes with time.
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ticking || !self.changes_with_time(cx) {
            return;
        }
        self.ticking = true;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let ticking = this.update_in(cx, |list, window, cx| {
                    list.advance(window, cx);
                    list.ticking = list.changes_with_time(cx);
                    list.ticking
                });
                if !matches!(ticking, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Whether a notification is entering, leaving, or counting down to its
    /// auto-dismiss. A countdown paused by an inactive window or an expanded
    /// stack still needs the clock: the manager measures each step from the
    /// previous one, so a stopped clock would charge the pause on resume.
    fn changes_with_time(&self, cx: &App) -> bool {
        self.manager.iter().any(|(_, note, status)| {
            status != ToastTransitionStatus::Present || self.compact || note.read(cx).autohide
        })
    }

    pub fn push(&mut self, note: Notification, window: &mut Window, cx: &mut Context<Self>) {
        let compact = crate::window_seam::window_is_compact(window, cx);
        self.sync_layout(compact, cx);
        let id = note.id.clone();
        // A notice pushed again under its id updates the one on screen, so it
        // neither replays its entry nor, if it was leaving, finishes leaving.
        let existing = self.manager.get(&id).cloned();
        let timeout = if compact {
            // Replacement is immediate: discarded messages never reappear later.
            self.manager = toast_manager(true);
            self.subscriptions
                .retain(|kept, _| existing.is_some() && *kept == id);
            Some(Duration::from_secs(if note.action.is_some() {
                5
            } else {
                3
            }))
        } else {
            note.autohide.then_some(Duration::from_secs(5))
        };
        let entity = match existing {
            Some(entity) => {
                entity.update(cx, |current, cx| {
                    let motion = current.motion.clone();
                    *current = note;
                    current.motion = motion;
                    cx.notify();
                });
                entity
            }
            None => {
                let entity = cx.new(|_| note);
                let dismiss_id = id.clone();
                self.subscriptions.insert(
                    id.clone(),
                    cx.subscribe_in(
                        &entity,
                        window,
                        move |this, _, _: &DismissRequest, window, cx| {
                            this.dismiss(&dismiss_id, window, cx);
                        },
                    ),
                );
                entity
            }
        };
        self.manager.push(
            id,
            entity,
            ToastOptions { timeout },
            cx.background_executor().now(),
        );
        self.tick(window, cx);
        cx.notify();
    }

    fn sync_layout(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact == compact {
            return;
        }
        self.compact = compact;
        let latest = self
            .manager
            .iter()
            .rev()
            .find(|(_, _, status)| *status != ToastTransitionStatus::Ending)
            .map(|(id, note, _)| (id.clone(), note.clone()));
        self.manager = toast_manager(compact);
        self.subscriptions
            .retain(|id, _| latest.as_ref().is_some_and(|(latest, _)| latest == id));
        if let Some((id, note)) = latest {
            let value = note.read(cx);
            let timeout = if compact {
                Some(Duration::from_secs(if value.action.is_some() {
                    5
                } else {
                    3
                }))
            } else {
                value.autohide.then_some(Duration::from_secs(5))
            };
            self.manager.push(
                id,
                note,
                ToastOptions { timeout },
                cx.background_executor().now(),
            );
        }
    }

    fn dismiss(&mut self, id: &NotificationId, window: &mut Window, cx: &mut Context<Self>) {
        if self.manager.dismiss(id, cx.background_executor().now())
            && let Some(note) = self.manager.get(id)
        {
            note.update(cx, |note, cx| {
                note.transition_status = ToastTransitionStatus::Ending;
                cx.notify();
            });
        }
        self.tick(window, cx);
        cx.notify();
    }

    fn advance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let changes = self.manager.advance(
            cx.background_executor().now(),
            !crate::window_seam::window_is_compact(window, cx)
                && (self.stack_state.is_expanded() || !window.is_window_active()),
        );
        for id in changes.presented {
            if let Some(note) = self.manager.get(&id) {
                note.update(cx, |note, cx| {
                    note.transition_status = ToastTransitionStatus::Present;
                    cx.notify();
                });
            }
        }
        for id in changes.ending {
            if let Some(note) = self.manager.get(&id) {
                note.update(cx, |note, cx| {
                    note.transition_status = ToastTransitionStatus::Ending;
                    cx.notify();
                });
            }
        }
        for (id, note) in changes.removed {
            self.subscriptions.remove(&id);
            note.update(cx, |_, cx| cx.emit(DismissEvent));
        }
        if changes.changed {
            cx.notify();
        }
    }

    /// Whether no notification is shown, leaving ones included.
    pub(super) fn is_empty(&self) -> bool {
        self.manager.is_empty()
    }

    /// Whether a notification of this type is shown and not already leaving.
    pub fn contains_type(&self, kind: TypeId) -> bool {
        self.manager.iter().any(|(id, _, status)| {
            status != ToastTransitionStatus::Ending
                && matches!(
                    id,
                    NotificationId::Type(value) | NotificationId::Key(value, _) if *value == kind
                )
        })
    }

    pub fn close(
        &mut self,
        id: impl Into<NotificationId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss(&id.into(), window, cx);
    }
    pub fn close_by_type(&mut self, kind: TypeId, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self
            .manager
            .iter()
            .filter_map(|(id, _, _)| match id {
                NotificationId::Type(value) | NotificationId::Key(value, _) if *value == kind => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for id in ids {
            self.dismiss(&id, window, cx);
        }
    }
    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.manager.dismiss_all(cx.background_executor().now());
        for id in ids {
            if let Some(note) = self.manager.get(&id) {
                note.update(cx, |note, cx| {
                    note.transition_status = ToastTransitionStatus::Ending;
                    cx.notify();
                });
            }
        }
        self.tick(window, cx);
        cx.notify();
    }
}

impl Render for NotificationList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::window_seam::window_is_compact(window, cx);
        self.sync_layout(compact, cx);
        self.tick(window, cx);
        if compact {
            return div()
                .max_w_full()
                .children(
                    self.manager
                        .iter()
                        .next_back()
                        .map(|(_, note, _)| note.clone()),
                )
                .into_any_element();
        }
        let items = self
            .manager
            .visible(5)
            .map(|(id, note, _)| (id.clone(), note.clone()))
            .collect::<Vec<_>>();
        items
            .into_iter()
            .fold(
                ToastStack::new("notification-list", self.stack_state.clone()),
                |stack, (id, note)| stack.item(format!("{id:?}"), note),
            )
            .focus_handle(self.focus_handle.clone())
            .w(DEFAULT_WIDTH)
            .max_h(window.viewport_size().height)
            .into_any_element()
    }
}

#[cfg(test)]
impl NotificationList {
    pub(super) fn assert_messages(&self, cx: &App, expected: &[&str]) {
        let messages = self
            .manager
            .iter()
            .map(|(_, note, _)| note.read(cx).message.as_ref().unwrap().as_ref())
            .collect::<Vec<_>>();
        assert_eq!(messages, expected);
    }
}
