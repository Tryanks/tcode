use super::*;
use crate::widgets::choice::{Choice, ChoiceGroup};
use tcode_core::permissions::ComputerUsePermissions;
use tcode_core::settings::{ForegroundApp, ForegroundChoice};

/// How long what is drawn here only some of the time takes to come in or go.
const SHOW: std::time::Duration = std::time::Duration::from_millis(200);

/// What Settings → Computer Use learns of the host it configures.
#[derive(Default)]
pub(super) struct ComputerUseSection {
    /// Whether the host is a Mac, the only host whose computer use brings
    /// an app to the front; unknown until the host answers.
    host_macos: Option<bool>,
    query: Option<gpui::Task<()>>,
}

impl ComputerUseSection {
    pub(super) fn hidden(&mut self) {
        self.host_macos = None;
        self.query = None;
    }
}

impl SettingsPage {
    fn ask_host_platform(&mut self, cx: &mut Context<Self>) {
        if self.computer_use.host_macos.is_some() || self.computer_use.query.is_some() {
            return;
        }
        let task = self
            .store
            .update(cx, |store, cx| store.computer_use_permissions(cx));
        self.computer_use.query = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.computer_use.host_macos =
                    Some(matches!(result, Ok(ComputerUsePermissions::MacOs(_))));
                cx.notify();
            });
        }));
    }

    /// The apps remembered as allowed or not to come to the front for
    /// typing, and the choice for every other app; drawn only for a Mac host.
    pub(super) fn foreground_group(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.ask_host_platform(cx);
        let shown = crate::motion::shown(
            "cu-foreground",
            (self.computer_use.host_macos == Some(true)).then_some(()),
            SHOW,
            window,
            cx,
        )?;
        let compact = self.window_state.read(cx).compact;
        let settings = self.store.read(cx).settings().computer_use;
        let mut rows: Vec<AnyElement> = settings
            .foreground_apps
            .iter()
            .map(|(bundle_id, app)| self.foreground_app_row(bundle_id, app, compact, cx))
            .collect();
        if rows.is_empty() {
            rows.push(
                crate::material::empty_row(crate::tr!("computer_use.foreground.empty"), cx)
                    .into_any_element(),
            );
        }
        let current = settings.foreground_default;
        let choices = [
            (ForegroundChoice::Ask, "computer_use.foreground.ask"),
            (ForegroundChoice::Allow, "computer_use.foreground.allow"),
            (ForegroundChoice::Deny, "computer_use.foreground.deny"),
        ]
        .into_iter()
        .map(|(choice, label)| {
            let page = cx.entity();
            Choice::new(
                SharedString::from(format!("cu-foreground-default-{choice:?}")),
                crate::tr!(label),
            )
            .selected(choice == current)
            .on_select(move |_, cx| {
                page.update(cx, |page, cx| {
                    page.dispatch_settings(move |store| store.set_foreground_default(choice), cx)
                })
            })
        });
        let reset = crate::settings_rows::reset_action(
            "reset-cu-foreground-default",
            current != ForegroundChoice::default(),
            cx.listener(|this, _, _, cx| {
                this.dispatch_settings(
                    |store| store.set_foreground_default(ForegroundChoice::default()),
                    cx,
                )
            }),
        );
        let row = self.flip_row("cu-foreground-default", cx);
        let other = row.row(
            crate::settings_rows::row_labels(
                crate::tr!("computer_use.foreground.other_apps"),
                None,
                reset,
                compact,
                cx,
            ),
            ChoiceGroup::new("cu-foreground-default")
                .compact(compact)
                .aria_label(crate::tr!("computer_use.foreground.other_apps"))
                .choices(choices),
            cx,
        );
        rows.push(row.render(other));
        Some(
            shown.fold(
                v_flex()
                    .debug_selector(|| "cu-foreground".into())
                    .child(crate::material::group_caption(
                        crate::tr!("computer_use.foreground.section"),
                        cx,
                    ))
                    .child(crate::material::grouped(rows, cx))
                    .child(crate::settings_rows::footnote(
                        crate::tr!("computer_use.foreground.footnote"),
                        compact,
                        cx,
                    )),
            ),
        )
    }

    fn foreground_app_row(
        &self,
        bundle_id: &str,
        app: &ForegroundApp,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let choices = [
            (true, "computer_use.foreground.always_allow"),
            (false, "computer_use.foreground.deny"),
        ]
        .into_iter()
        .map(|(allow, label)| {
            let page = cx.entity();
            let (bundle_id, name) = (bundle_id.to_owned(), app.name.clone());
            Choice::new(
                SharedString::from(format!("cu-foreground-app-{bundle_id}-{allow}")),
                crate::tr!(label),
            )
            .selected(app.allow == allow)
            .on_select(move |_, cx| {
                let (bundle_id, name) = (bundle_id.clone(), name.clone());
                page.update(cx, |page, cx| {
                    page.dispatch_settings(
                        move |store| {
                            store.set_foreground_app(bundle_id, Some(ForegroundApp { name, allow }))
                        },
                        cx,
                    )
                })
            })
        });
        let forget = {
            let bundle_id = bundle_id.to_owned();
            Button::new(SharedString::from(format!(
                "cu-foreground-forget-{bundle_id}"
            )))
            .ghost()
            .xsmall()
            .icon(IconName::Close)
            .tooltip(crate::tr!(
                "computer_use.foreground.forget",
                app = app.name.as_str()
            ))
            .aria_label(crate::tr!(
                "computer_use.foreground.forget",
                app = app.name.as_str()
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                let bundle_id = bundle_id.clone();
                this.dispatch_settings(move |store| store.set_foreground_app(bundle_id, None), cx)
            }))
        };
        let row = self.flip_row(format!("cu-foreground-app-{bundle_id}"), cx);
        let layout = row.row(
            crate::settings_rows::row_labels(
                app.name.clone(),
                Some(bundle_id.to_owned().into()),
                None,
                compact,
                cx,
            ),
            gpui_base::h_flex()
                .gap_2()
                .items_center()
                .child(
                    ChoiceGroup::new(SharedString::from(format!("cu-foreground-app-{bundle_id}")))
                        .compact(compact)
                        .aria_label(app.name.clone())
                        .choices(choices),
                )
                .child(forget),
            cx,
        );
        row.render(layout)
    }
}
