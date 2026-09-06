//! The macOS TCC permission group in Settings → Computer Use.
//!
//! This is the one genuinely native part of that page: reading Accessibility and
//! Screen Recording grants, asking for them, and relaunching afterwards. It is
//! compiled only on macOS with `local-permissions`, and instantiated only for a
//! *local* attachment — a client can grant permissions on its own machine and
//! nowhere else. Everything else on the page (the Computer Use switches) is
//! host configuration and stays editable over a remote link.

use computer_use_mcp::permissions::{
    self, PermissionGrantAction, PermissionGrantFlow, PermissionKind, PermissionStatus,
    open_settings_pane, relaunch_app, request,
};
use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    Subscription, Window, div, px,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};

use crate::icon::{Icon, IconName};
use crate::shell::Quit;
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::widgets::button::{Button, ButtonVariants as _};

const KINDS: [PermissionKind; 2] = [
    PermissionKind::Accessibility,
    PermissionKind::ScreenRecording,
];

pub(crate) struct LocalPermissions {
    store: Entity<WorkspaceStore>,
    /// Last-known TCC snapshot, refreshed on window activation and on every
    /// explicit Grant / Recheck.
    status: PermissionStatus,
    /// A fresh Screen Recording grant only takes effect after tcode relaunches;
    /// this drives the restart banner.
    restart_hint: bool,
    /// A temporary continuity marker exists for an in-flight Screen Recording
    /// request. Cleared when the app becomes active without a grant.
    marker_pending: bool,
    /// Separates the initial native TCC request from the explicit fallback that
    /// opens System Settings when macOS will no longer show its prompt.
    grant_flow: PermissionGrantFlow,
    _activation: Subscription,
}

impl LocalPermissions {
    pub(crate) fn new(
        store: Entity<WorkspaceStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Returning to tcode is how a grant made in System Settings becomes
        // visible, so window activation is the recheck trigger. It is also the
        // moment a grant the user declined must clear its continuity marker.
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.recheck(true, cx);
            }
        });
        Self {
            store,
            // Refresh once as the component mounts. When the page was opened by
            // a post-grant relaunch this is the automatic recheck that surfaces
            // the new status immediately.
            status: permissions::check(),
            restart_hint: false,
            marker_pending: false,
            grant_flow: PermissionGrantFlow::default(),
            _activation: activation,
        }
    }

    /// Fire the native prompt first. If the permission remains missing, the next
    /// explicit click opens System Settings as a fallback; doing both at once
    /// races macOS's own consent dialog and duplicates its Open Settings action.
    fn grant(&mut self, kind: PermissionKind, cx: &mut Context<Self>) {
        match self.grant_flow.advance(kind) {
            PermissionGrantAction::Request => {
                if kind == PermissionKind::ScreenRecording {
                    self.store.update(cx, |store, _cx| {
                        store.write_relaunch_marker("computer_use".into());
                    });
                    self.marker_pending = true;
                }
                let _ = request(kind);
                // Both native request APIs may return before the user has
                // completed the system UI, so this immediate snapshot must not
                // clear the temporary Screen Recording marker.
                self.recheck(false, cx);
            }
            PermissionGrantAction::OpenSettings => {
                open_settings_pane(kind);
                cx.notify();
            }
        }
    }

    fn recheck(&mut self, clear_ungranted_marker: bool, cx: &mut Context<Self>) {
        let fresh = permissions::check();
        if clear_ungranted_marker && self.marker_pending && !fresh.screen_recording {
            self.store.update(cx, |store, _cx| {
                store.clear_relaunch_marker();
            });
            self.marker_pending = false;
        }
        // A Screen Recording grant that flips on still needs a restart to take
        // effect for the running process, so surface the relaunch affordance.
        if fresh.screen_recording && !self.status.screen_recording {
            self.restart_hint = true;
        }
        self.status = fresh;
        cx.notify();
    }

    fn relaunch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _cx| {
            store.write_relaunch_marker("computer_use".into());
        });
        if let Err(error) = relaunch_app() {
            log::warn!("failed to relaunch tcode: {error}");
            return;
        }
        // Quit through the app's existing quit action; the fresh instance
        // consumes the marker on launch.
        window.dispatch_action(Box::new(Quit), cx);
    }

    fn permission_row(&self, kind: PermissionKind, cx: &mut Context<Self>) -> AnyElement {
        let granted = self.status.granted(kind);
        let grant_label = match self.grant_flow.action(kind) {
            PermissionGrantAction::Request => crate::tr!("permissions.grant"),
            PermissionGrantAction::OpenSettings => crate::tr!("permissions.open_settings"),
        };
        let (name_key, why_key, grant_id, recheck_id) = match kind {
            PermissionKind::Accessibility => (
                "permissions.accessibility.name",
                "permissions.accessibility.why",
                "perm-grant-accessibility",
                "perm-recheck-accessibility",
            ),
            PermissionKind::ScreenRecording => (
                "permissions.screen_recording.name",
                "permissions.screen_recording.why",
                "perm-grant-screen-recording",
                "perm-recheck-screen-recording",
            ),
        };
        let (bg, fg, chip) = if granted {
            (
                cx.theme().success.opacity(0.12),
                cx.theme().success_foreground,
                crate::tr!("permissions.granted"),
            )
        } else {
            (
                cx.theme().warning.opacity(0.12),
                cx.theme().warning_foreground,
                crate::tr!("permissions.missing"),
            )
        };
        let mut controls = h_flex()
            .flex_none()
            .gap_2()
            .items_center()
            .child(crate::material::semantic_chip(chip, bg, fg));
        if !granted {
            controls = controls
                .child(
                    Button::new(grant_id)
                        .outline()
                        .small()
                        .label(grant_label)
                        .on_click(cx.listener(move |this, _, _, cx| this.grant(kind, cx))),
                )
                .child(
                    Button::new(recheck_id)
                        .ghost()
                        .small()
                        .label(crate::tr!("permissions.recheck"))
                        .on_click(cx.listener(|this, _, _, cx| this.recheck(true, cx))),
                );
        }
        // No reset affordance: the grant lives in the OS, not in settings.json.
        h_flex()
            .w_full()
            .min_h(px(44.))
            .px_3()
            .py_2p5()
            .gap_3()
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_medium()
                            .child(crate::tr!(name_key)),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::tr!(why_key)),
                    ),
            )
            .child(controls)
            .into_any_element()
    }

    fn restart_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .rounded(crate::material::radius_card())
            .bg(cx.theme().warning.opacity(0.12))
            .px_3()
            .py_2p5()
            .child(
                Icon::new(IconName::Info)
                    .small()
                    .text_color(cx.theme().warning_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .child(crate::tr!("permissions.restart_banner")),
            )
            .child(
                Button::new("perm-relaunch")
                    .outline()
                    .small()
                    .label(crate::tr!("permissions.relaunch"))
                    .on_click(cx.listener(|this, _, window, cx| this.relaunch(window, cx))),
            )
            .into_any_element()
    }
}

impl Render for LocalPermissions {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = KINDS
            .iter()
            .map(|kind| self.permission_row(*kind, cx))
            .collect();
        v_flex()
            .w_full()
            .gap_2()
            .child(crate::material::grouped(rows, cx))
            .children(self.restart_hint.then(|| self.restart_banner(cx)))
    }
}
