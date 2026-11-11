use super::*;
use gpui::{Hsla, relative};
use gpui_base::h_flex;
use tcode_protocol::{AccountDiskUsage, DiskUsage, ProviderDiskUsage};

use crate::thread_export::format_size;

/// The height of the bar sharing out Tcode's total.
const BAR_HEIGHT: f32 = 6.;
/// The diameter of a legend's colour key.
const KEY: f32 = 8.;

impl SettingsPage {
    /// What the host's data and its providers' take on its disk: Tcode's own
    /// total shared out by part, then each provider's.
    pub(super) fn disk_usage_groups(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let compact = self.window_state.read(cx).compact;
        let state = self.store.read(cx).disk_usage().clone();
        let Some(usage) = state.last else {
            let card = crate::material::group(cx)
                .debug_selector(|| "disk-usage-measuring".into())
                .child(crate::material::empty_row(
                    crate::tr!("settings.disk_usage.measuring"),
                    cx,
                ));
            return vec![
                v_flex()
                    .child(self.disk_usage_header(true, None, compact, cx))
                    .child(card)
                    .into_any_element(),
            ];
        };
        let mut groups = vec![
            v_flex()
                .child(self.disk_usage_header(state.measuring, Some(&usage), compact, cx))
                .child(crate::material::grouped(
                    vec![self.tcode_usage(&usage, compact, cx)],
                    cx,
                ))
                .child(crate::settings_rows::footnote(
                    crate::tr!("settings.disk_usage.caption", dir = usage.tcode.dir.clone())
                        .into_owned(),
                    compact,
                    cx,
                ))
                .into_any_element(),
        ];
        if !usage.providers.is_empty() {
            let rows = usage
                .providers
                .iter()
                .map(|provider| self.provider_usage(provider, compact, window, cx))
                .collect();
            groups.push(
                v_flex()
                    .child(crate::material::group_caption(
                        crate::tr!("settings.disk_usage.providers_section"),
                        cx,
                    ))
                    .child(crate::material::grouped(rows, cx))
                    .into_any_element(),
            );
        }
        groups
    }

    /// The group's caption, with when it was last measured and the way to
    /// measure again; a measurement running says so instead. Before the
    /// first one ends, the card below says it runs.
    fn disk_usage_header(
        &self,
        measuring: bool,
        usage: Option<&DiskUsage>,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let status = usage.map(|usage| {
            if measuring {
                h_flex()
                    .gap_1()
                    .debug_selector(|| "disk-usage-remeasuring".into())
                    .child(crate::widgets::spinner::Spinner::new().xsmall())
                    .child(crate::tr!("settings.disk_usage.measuring").into_owned())
            } else {
                let ago = humanize_ago(
                    self.store
                        .read(cx)
                        .host_now_secs(cx)
                        .saturating_sub(usage.measured_at),
                );
                h_flex().child(crate::tr!("settings.disk_usage.measured", ago = ago).into_owned())
            }
        });
        h_flex()
            .w_full()
            .items_end()
            .justify_between()
            .gap_3()
            .pb(cx.theme().tokens.spacing.sm)
            .child(
                crate::material::group_caption(crate::tr!("settings.disk_usage.section"), cx)
                    .pb_0(),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .text_secondary(compact, cx)
                    .children(status)
                    .when(usage.is_some(), |row| {
                        row.child(
                            Button::new("disk-usage-measure-again")
                                .ghost()
                                .xsmall()
                                .label(crate::tr!("settings.disk_usage.measure_again"))
                                .disabled(measuring)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.store.update(cx, |store, _| store.measure_disk_usage());
                                })),
                        )
                    }),
            )
    }

    /// Tcode's data directory: its total, a bar sharing it out by part, and
    /// a legend keying the bar.
    fn tcode_usage(&self, usage: &DiskUsage, compact: bool, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let tcode = &usage.tcode;
        let parts: [(&str, SharedString, u64, Hsla); 4] = [
            (
                "conversations",
                crate::tr!("settings.disk_usage.conversations").into(),
                tcode.conversations,
                theme.primary,
            ),
            (
                "images",
                crate::tr!("settings.disk_usage.images").into(),
                tcode.images,
                theme.warning,
            ),
            (
                "worktrees",
                crate::tr!("settings.disk_usage.worktrees").into(),
                tcode.worktrees,
                theme.success,
            ),
            (
                "other",
                crate::tr!("settings.disk_usage.other").into(),
                tcode.other,
                theme.muted_foreground,
            ),
        ];
        let total = tcode.total();
        // gpui-base's progress primitives draw one indicator on a track; a
        // bar sharing a whole among several parts lays one per part along it.
        let height = px(BAR_HEIGHT);
        let bar =
            gpui_base::ProgressTrack::new()
                .w_full()
                .h(height)
                .rounded(height / 2.)
                .overflow_hidden()
                .bg(theme.muted)
                .child(h_flex().size_full().children(
                    parts.iter().filter(|(_, _, bytes, _)| *bytes > 0).map(
                        |(_, _, bytes, color)| {
                            gpui_base::ProgressIndicator::new()
                                .h_full()
                                .w(relative(*bytes as f32 / total.max(1) as f32))
                                .bg(*color)
                        },
                    ),
                ));
        let legend =
            h_flex()
                .w_full()
                .flex_wrap()
                .gap_x_4()
                .gap_y_1()
                .children(parts.into_iter().map(|(key, label, bytes, color)| {
                    h_flex()
                        .debug_selector(move || format!("disk-usage-tcode-{key}"))
                        .gap_1p5()
                        .items_center()
                        .text_secondary(compact, cx)
                        .child(div().flex_none().size(px(KEY)).rounded_full().bg(color))
                        .child(format!("{label} {}", format_size(bytes)))
                }));
        crate::material::group_row_stacked(cx)
            .debug_selector(|| "disk-usage-tcode".into())
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .gap_3()
                    .text_body(compact, cx)
                    .child(crate::tr!("settings.disk_usage.tcode").into_owned())
                    .child(div().flex_none().child(format_size(total))),
            )
            .child(bar)
            .child(legend)
            .into_any_element()
    }

    /// One provider's directories as they are; it unfolds into its accounts
    /// when it has several, else into its account's largest entries.
    fn provider_usage(
        &self,
        provider: &ProviderDiskUsage,
        compact: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let kind = provider.provider;
        let several = provider.accounts.len() > 1;
        let only = provider.accounts.first().filter(|_| !several);
        let detail = match only {
            Some(account) => account.dirs.join(", "),
            None => crate::tr!(
                "settings.disk_usage.accounts",
                count = provider.accounts.len()
            )
            .into_owned(),
        };
        let unfolds = several || only.is_some_and(|account| !account.parts.is_empty());
        let open = unfolds && self.disk_usage_open.contains(&kind);
        let key = tcode_core::settings::provider_key(kind);
        let name = kind.display_name();
        let header = h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .child(div().flex_none().w(px(16.)).when(unfolds, |slot| {
                slot.child(
                    crate::widgets::Chevron::owned_icon(
                        &self.motion,
                        SharedString::from(format!("disk-usage-chevron-{key}")),
                        open,
                        window,
                        cx,
                    )
                    .text_color(cx.theme().muted_foreground),
                )
            }))
            .child(crate::settings_rows::row_labels(
                name,
                Some(detail.into()),
                None,
                compact,
                cx,
            ))
            .child(
                div()
                    .flex_none()
                    .text_body(compact, cx)
                    .child(format_size(provider.bytes)),
            );
        let header = if unfolds {
            crate::material::accessible_clickable(
                header,
                SharedString::from(format!("disk-usage-provider-{key}")),
                Role::Button,
                name,
                cx,
            )
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.disk_usage_open.remove(&kind) {
                    this.disk_usage_open.insert(kind);
                }
                cx.notify();
            }))
            .into_any_element()
        } else {
            header.into_any_element()
        };
        let lines: Vec<(SharedString, Option<String>, u64)> = if several {
            let settings = self.store.read(cx).settings();
            provider
                .accounts
                .iter()
                .map(|account| {
                    (
                        settings.profile_display_name(&account.profile_id).into(),
                        Some(account.dirs.join(", ")),
                        account.bytes,
                    )
                })
                .collect()
        } else {
            only.map(parts).unwrap_or_default()
        };
        let spacing = cx.theme().tokens.spacing.sm;
        let lines = v_flex()
            .w_full()
            .pt(spacing)
            .children(lines.into_iter().map(|(label, detail, bytes)| {
                h_flex()
                    .w_full()
                    .gap_3()
                    .py_1()
                    .pl(px(16.) + cx.theme().tokens.spacing.md)
                    .items_center()
                    .child(crate::settings_rows::row_labels(
                        label,
                        detail.map(SharedString::from),
                        None,
                        compact,
                        cx,
                    ))
                    .child(
                        div()
                            .flex_none()
                            .text_secondary(compact, cx)
                            .child(format_size(bytes)),
                    )
            }));
        crate::material::group_row_stacked(cx)
            .debug_selector(move || format!("disk-usage-provider-{key}"))
            .child(header)
            .child(
                crate::widgets::Collapsible::new(
                    SharedString::from(format!("disk-usage-fold-{key}")),
                    open,
                )
                .motion(&self.motion)
                .w_full()
                .content(lines),
            )
            .into_any_element()
    }
}

/// An account's largest entries, and what the rest of it takes.
fn parts(account: &AccountDiskUsage) -> Vec<(SharedString, Option<String>, u64)> {
    let mut lines: Vec<_> = account
        .parts
        .iter()
        .map(|part| (SharedString::from(part.name.clone()), None, part.bytes))
        .collect();
    let named: u64 = account.parts.iter().map(|part| part.bytes).sum();
    let rest = account.bytes.saturating_sub(named);
    if rest > 0 {
        lines.push((crate::tr!("settings.disk_usage.rest").into(), None, rest));
    }
    lines
}
