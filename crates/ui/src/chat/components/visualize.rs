//! A visualization's card in the conversation: the page a `visualize` call
//! asked for, drawn under its title, with its HTML to read and to copy.

use std::sync::{Arc, Weak};

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _,
    IntoElement as _, ParentElement as _, SharedString, Styled as _, Subscription, Task, Window,
    div, prelude::FluentBuilder as _,
};
use gpui_base::{StyledExt as _, h_flex, v_flex};
use tcode_client::original::OriginalTransfer;
use tcode_core::session::TimelineEntry;
use tcode_protocol::ProtocolError;

use crate::html::{HtmlState, HtmlView};
use crate::icon::{Icon, IconName};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::Spinner;
use crate::widgets::copy::{action_button, copy_button};
use crate::widgets::menu::{ContextMenuExt as _, CopyText};

const MAX_HTML_BYTES: usize = agent::McpRegistration::VISUALIZE_MAX_HTML_BYTES;

/// A visualization's page as the timeline keeps it while its row is near the
/// screen.
pub(crate) struct Visual {
    pub(crate) state: Entity<HtmlState>,
    /// The HTML `state` draws, which the card shows as source and copies.
    pub(crate) html: Arc<str>,
    /// The entry the page was last taken from; see `MdState`'s field of the
    /// same name.
    synced_from: Weak<TimelineEntry>,
    /// The read of the page's file, for a call that names one.
    pub(crate) file: Option<FileRead>,
    _redraw: Subscription,
}

/// Where the read of a page's file from the thread's host stands.
pub(crate) enum FileRead {
    /// Dropping the task stops the read.
    Reading {
        _read: Task<()>,
    },
    Read,
    Failed(Unread),
}

/// Why a page's file was not read.
pub(crate) enum Unread {
    /// What the host answered.
    Host(String),
    TooLarge,
    NotText,
}

impl Unread {
    /// What the card says in the page's place.
    pub(crate) fn message(&self, path: &str) -> String {
        match self {
            Self::Host(error) => {
                crate::tr!("chat.visualize_unreadable", path = path, error = error)
            }
            Self::TooLarge => crate::tr!(
                "chat.visualize_too_large",
                path = path,
                limit = crate::thread_export::format_size(MAX_HTML_BYTES as u64)
            ),
            Self::NotText => crate::tr!("chat.visualize_not_text", path = path),
        }
        .into_owned()
    }
}

/// The page in the file `transfer` reads from the thread's host, within the
/// size the tool takes.
pub(crate) async fn read_page(mut transfer: OriginalTransfer) -> Result<String, Unread> {
    let host = |error: ProtocolError| Unread::Host(error.message);
    if transfer.metadata().await.map_err(host)?.len > MAX_HTML_BYTES as u64 {
        return Err(Unread::TooLarge);
    }
    let mut bytes = Vec::new();
    // The length was the file's when it was opened; what arrives is counted too.
    while let Some(block) = transfer.read().await.map_err(host)? {
        if bytes.len() + block.len() > MAX_HTML_BYTES {
            return Err(Unread::TooLarge);
        }
        bytes.extend_from_slice(&block);
    }
    String::from_utf8(bytes).map_err(|_| Unread::NotText)
}

impl Visual {
    /// The page of `html`, whose `owner` draws again as its pictures arrive.
    pub(crate) fn new<T: 'static>(html: &str, owner: &mut Context<T>) -> Self {
        let html: Arc<str> = Arc::from(html);
        let state = owner.new(|cx| HtmlState::new(html.clone(), cx));
        let redraw = owner.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            html,
            synced_from: Weak::new(),
            file: None,
            _redraw: redraw,
        }
    }

    pub(crate) fn set_html(&mut self, html: &str, cx: &mut App) {
        if *self.html != *html {
            self.html = Arc::from(html);
            self.state.update(cx, |state, cx| state.set_html(html, cx));
        }
    }

    /// Whether `entry` is the one the page was last taken from.
    pub(crate) fn synced_from(&self, entry: &Arc<TimelineEntry>) -> bool {
        std::ptr::eq(self.synced_from.as_ptr(), Arc::as_ptr(entry))
    }

    pub(crate) fn set_synced_from(&mut self, entry: &Arc<TimelineEntry>) {
        self.synced_from = Arc::downgrade(entry);
    }
}

/// What a card shows under its header.
pub(crate) enum CardBody {
    /// No page yet: the call is still running, or its file is being read.
    Building,
    /// The call's file could not be read.
    Unreadable(String),
    /// The page, whose HTML is `html`. `source` is that HTML as code, when the
    /// reader asked for it in place of the drawing.
    Page {
        state: Entity<HtmlState>,
        html: Arc<str>,
        source: Option<AnyElement>,
    },
}

pub(crate) struct VisualizeCardData<'a> {
    pub(crate) id: &'a str,
    /// Empty when the call names none.
    pub(crate) title: &'a str,
    pub(crate) description: Option<&'a str>,
    pub(crate) body: CardBody,
    pub(crate) running: bool,
    pub(crate) collapsed: bool,
    pub(crate) copied: bool,
    pub(crate) compact: bool,
    pub(crate) viewport_width: Option<f32>,
    pub(crate) motion: crate::motion::owned::Owned,
}

pub(crate) fn visualize_card(
    data: VisualizeCardData<'_>,
    on_toggle_collapse: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_toggle_source: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    on_copy: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let VisualizeCardData {
        id,
        title,
        description,
        body,
        running,
        collapsed,
        copied,
        compact,
        viewport_width,
        motion,
    } = data;

    let theme = cx.theme();
    let radius = crate::material::radius_card(cx);
    let page = match &body {
        CardBody::Page { html, source, .. } => Some((html.clone(), source.is_some())),
        CardBody::Building | CardBody::Unreadable(_) => None,
    };

    let (collapse_icon, collapse_label) = if collapsed {
        (IconName::ChevronDown, crate::tr!("chat.visualize_expand"))
    } else {
        (IconName::ChevronUp, crate::tr!("chat.visualize_collapse"))
    };

    let collapse_btn = {
        let selector = format!("visualize-collapse-{id}");
        action_button(
            format!("visualize-collapse-{id}"),
            collapse_icon,
            collapse_label.into_owned(),
            cx,
        )
        .touch(compact)
        .motion(&motion)
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_toggle_collapse(event, window, cx);
        })
        .debug_selector(move || selector)
    };

    let toggle_btn = page
        .as_ref()
        .filter(|_| !collapsed)
        .map(|(_, showing_source)| {
            let (icon, label) = if *showing_source {
                (IconName::Eye, crate::tr!("chat.visualize_show_rendered"))
            } else {
                (IconName::Code, crate::tr!("chat.visualize_show_source"))
            };
            let selector = format!("visualize-toggle-{id}");
            action_button(
                format!("visualize-toggle-{id}"),
                icon,
                label.into_owned(),
                cx,
            )
            .touch(compact)
            .motion(&motion)
            .on_click(move |event, window, cx| {
                cx.stop_propagation();
                on_toggle_source(event, window, cx);
            })
            .debug_selector(move || selector)
        });

    let copy_btn = page.is_some().then(|| {
        let selector = format!("copy-visualize-{id}");
        copy_button(
            &format!("visualize-{id}"),
            copied,
            compact,
            move |event, window, cx| {
                cx.stop_propagation();
                on_copy(event, window, cx);
            },
            cx,
        )
        .motion(&motion)
        .debug_selector(move || selector)
    });

    let header = h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .pl_3()
        .pr_1p5()
        .py_1p5()
        .when(collapsed, |bar| bar.rounded(radius))
        .when(!collapsed, |bar| {
            bar.rounded_t(radius)
                .border_b_1()
                .border_color(theme.border)
        })
        .bg(theme.tokens.colors.muted)
        .child(if running {
            Spinner::new().small().into_any_element()
        } else {
            Icon::new(IconName::LayoutDashboard)
                .small()
                .text_color(theme.muted_foreground)
                .into_any_element()
        })
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .items_baseline()
                .when(!title.is_empty(), |row| {
                    row.child(
                        div()
                            .text_body(false, cx)
                            .font_semibold()
                            .truncate()
                            .child(title.to_string()),
                    )
                })
                .when_some(description, |row, desc| {
                    row.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_secondary(false, cx)
                            .truncate()
                            .child(desc.to_string()),
                    )
                }),
        )
        .child(
            h_flex()
                .flex_shrink_0()
                .items_center()
                .gap_0p5()
                .children(toggle_btn)
                .children(copy_btn)
                .child(collapse_btn),
        );

    let note = |selector: String, text: String| {
        div()
            .w_full()
            .min_w_0()
            .p_4()
            .text_secondary(false, cx)
            .whitespace_normal()
            .child(text)
            .debug_selector(move || selector)
            .into_any_element()
    };
    let body = (!collapsed).then(|| match body {
        CardBody::Building => note(
            format!("visualize-building-{id}"),
            crate::tr!("chat.visualize_building").into_owned(),
        ),
        CardBody::Unreadable(why) => note(format!("visualize-unreadable-{id}"), why),
        CardBody::Page {
            source: Some(source),
            ..
        } => div()
            .w_full()
            .min_w_0()
            .p_3()
            .child(source)
            .into_any_element(),
        CardBody::Page {
            state,
            source: None,
            ..
        } => {
            let mut view = HtmlView::new(&state);
            if let Some(width) = viewport_width {
                view = view.viewport_width(width);
            }
            let selector = format!("visualize-page-{id}");
            div()
                .w_full()
                .min_w_0()
                .p_3()
                .child(view)
                .debug_selector(move || selector)
                .into_any_element()
        }
    });

    let card_id = SharedString::from(format!("visualize-card-{id}"));
    let selector = format!("visualize-card-{id}");
    let card = v_flex()
        .id(card_id)
        .w_full()
        .min_w_0()
        .rounded(radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .overflow_hidden()
        .child(header)
        .children(body)
        .debug_selector(move || selector);
    match page {
        Some((html, _)) => card
            .context_menu(move |menu, _, _| {
                menu.menu_with_icon(
                    crate::tr!("chat.copy_html").into_owned(),
                    IconName::Copy,
                    Box::new(CopyText(html.to_string())),
                )
            })
            .into_any_element(),
        None => card.into_any_element(),
    }
}
