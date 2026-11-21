//! A visualization in the conversation: the page a `visualize` call asked
//! for, drawn into the transcript, and the read of its file from the host.

use std::sync::{Arc, Weak};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, SharedString, Styled as _, Subscription, Task, div,
};
use gpui_base::h_flex;
use tcode_client::original::OriginalTransfer;
use tcode_core::session::TimelineEntry;
use tcode_protocol::ProtocolError;

use crate::html::{HtmlState, HtmlView};
use crate::icon::IconName;
use crate::sizing::Sizable as _;
use crate::typography::TextRole as _;
use crate::widgets::Spinner;
use crate::widgets::menu::{ContextMenuExt as _, CopyText};

const MAX_HTML_BYTES: usize = agent::McpRegistration::VISUALIZE_MAX_HTML_BYTES;

/// A visualization's page as the timeline keeps it while its row is near the
/// screen.
pub(crate) struct Visual {
    pub(crate) state: Entity<HtmlState>,
    /// The HTML `state` draws, which the context menu copies.
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
    /// What is said in the page's place.
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
        let state = owner.new(|_| HtmlState::new(html.clone()));
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

/// What stands in the conversation for a visualization.
pub(crate) enum Shown {
    /// No page yet: the call is still running, or its file is being read.
    Building,
    /// The call's file could not be read.
    Unreadable(String),
    /// The page, whose HTML is `html`.
    Page {
        state: Entity<HtmlState>,
        html: Arc<str>,
    },
}

/// A visualization in the conversation: its page drawn straight into the
/// transcript as a reply's own text is, with no frame, title or controls
/// around it. Its HTML is copied from the context menu.
pub(crate) fn visualization(
    id: &str,
    shown: Shown,
    viewport_width: Option<f32>,
    cx: &App,
) -> AnyElement {
    let note = |text: String| {
        div()
            .w_full()
            .min_w_0()
            .text_secondary(false, cx)
            .whitespace_normal()
            .child(text)
            .into_any_element()
    };
    match shown {
        Shown::Building => h_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(Spinner::new().small())
            .child(note(crate::tr!("chat.visualize_building").into_owned()))
            .into_any_element(),
        Shown::Unreadable(why) => note(why),
        Shown::Page { state, html } => {
            let mut view = HtmlView::new(&state);
            if let Some(width) = viewport_width {
                view = view.viewport_width(width);
            }
            div()
                .id(SharedString::from(format!("visualize-page-{id}")))
                .w_full()
                .min_w_0()
                .child(view)
                .context_menu(move |menu, _, _| {
                    menu.menu_with_icon(
                        crate::tr!("chat.copy_html").into_owned(),
                        IconName::Copy,
                        Box::new(CopyText(html.to_string())),
                    )
                })
                .into_any_element()
        }
    }
}
