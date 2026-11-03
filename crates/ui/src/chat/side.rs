//! An agent opened from its lead, shown inside the lead's own chat: beside
//! the lead's timeline and composer where the body has room for both, over
//! them otherwise. The lead's header stays; the agent has a header of its
//! own. The store records which agent is beside which lead
//! ([`WorkspaceStore::side_thread_for`]); the lead's chat hosts it, holding
//! the agent's thread open and its timeline leased while it draws it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, SharedString, Styled as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px, relative,
};
use gpui_base::{ResizableState, h_flex};

use super::ChatView;
use crate::icon::IconName;
use crate::motion::resize_glide::{Panel, ResizeGlide};
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

gpui::actions!(tcode_chat, [CloseSide]);

/// The key context of an agent shown beside its lead, where Escape closes it.
pub(crate) const SIDE_CONTEXT: &str = "ChatSide";

/// The narrowest an agent beside its lead reads at: a side panel's narrowest
/// content. The body splits once it holds three, so the lead keeps two.
const SIDE_MIN_WIDTH: f32 = crate::material::PANEL_CONTENT_MIN_WIDTH;

/// The share of the body an agent beside its lead takes until its divider is
/// dragged.
const SIDE_SHARE: f32 = 1. / 3.;

/// How long the agent takes to slide over the lead, or off it.
const SLIDE: Duration = Duration::from_millis(250);

/// How long one agent beside the lead takes to cross-fade into another.
const SWAP: Duration = Duration::from_millis(200);

/// How long what the side header shows only some of the time takes to come
/// in or go.
const SHOW: Duration = Duration::from_millis(200);

/// How long the agent's status label takes to cross-fade to another.
const CHANGE: Duration = Duration::from_millis(200);

/// Where an agent beside its lead is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SideMode {
    /// In the right part of the body, beside the lead's timeline.
    Split,
    /// Over the lead's timeline and composer, under the lead's header.
    Overlay,
}

impl SideMode {
    /// Where an agent goes in a body `width` wide.
    fn of(width: Pixels) -> Self {
        if width >= px(3. * SIDE_MIN_WIDTH) {
            Self::Split
        } else {
            Self::Overlay
        }
    }
}

/// An agent's chat hosted by its lead's.
struct SideHost {
    session_id: String,
    /// The lead's thread it was opened beside.
    lead: String,
    chat: Entity<ChatView>,
    /// Whether the host opened the thread, and so closes it.
    opened: bool,
}

/// What the lead's chat keeps of the agent beside it.
pub(super) struct Side {
    /// The agent shown.
    host: Option<SideHost>,
    /// The agent last shown, drawn until it has slid, shrunk or faded away.
    leaving: Option<SideHost>,
    /// The split of the lead's body with the agent, gliding it in and out.
    split: Entity<ResizableState>,
    glide: ResizeGlide,
    /// The agent's share of the split, as its divider was last dragged; kept
    /// by the view only, never saved.
    share: Rc<Cell<f32>>,
    /// Where the agent drawn was last drawn: a leaving one stays there.
    slot: SideMode,
    sync_pending: bool,
}

impl Side {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            host: None,
            leaving: None,
            split: cx.new(|_| ResizableState::default()),
            glide: ResizeGlide::new("chat-side-split", gpui::Axis::Horizontal),
            share: Rc::new(Cell::new(SIDE_SHARE)),
            slot: SideMode::Split,
            sync_pending: false,
        }
    }

    /// The lead shows another thread: its split lands where it is.
    pub(super) fn start_scene(&mut self) {
        self.glide.reset();
    }
}

/// What makes a chat an agent's shown beside its lead.
pub(super) struct SideOf {
    lead: WeakEntity<ChatView>,
    lead_session: String,
    /// Where the lead draws it, which its header follows.
    mode: SideMode,
}

impl ChatView {
    /// A chat for `session_id` shown beside `lead`, which hosts it: no
    /// header of the lead's kind, no terminal drawer, no side of its own.
    fn new_side(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<crate::window_state::WindowState>,
        lead: WeakEntity<ChatView>,
        lead_session: String,
        session_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut chat = Self::new(workspace_store, window_state, window, cx);
        chat.side_of = Some(SideOf {
            lead,
            lead_session,
            mode: SideMode::Split,
        });
        chat.set_session(Some(session_id), cx);
        chat
    }

    /// Whether the view is an agent's shown beside its lead.
    pub(super) fn is_side(&self) -> bool {
        self.side_of.is_some()
    }

    /// The agent shown beside the thread.
    pub(crate) fn side_session_id(&self) -> Option<&str> {
        self.side.host.as_ref().map(|host| host.session_id.as_str())
    }

    /// Whether an agent shows beside the thread in a body `width` wide,
    /// taking the room the side rail would have.
    pub(super) fn side_open(&self, width: Pixels, cx: &App) -> bool {
        self.side.host.is_some()
            && !self.window_state.read(cx).compact
            && SideMode::of(width) == SideMode::Split
    }

    /// The agent the store records beside this view's thread, where the
    /// view hosts one: never on an agent's own view, nor in the compact
    /// layout, which shows one thread at a time.
    fn wanted_side(&self, cx: &App) -> Option<String> {
        if self.side_of.is_some() || self.window_state.read(cx).compact {
            return None;
        }
        let lead = self.session_id.as_deref()?;
        self.workspace_store
            .read(cx)
            .side_thread_for(lead)
            .filter(|side| side != lead)
    }

    /// Whether the agent hosted, beside the thread it was opened beside, is
    /// the one [`Self::wanted_side`] names.
    fn side_in_sync(&self, cx: &App) -> bool {
        let hosted = self
            .side
            .host
            .as_ref()
            .map(|host| (host.lead.as_str(), host.session_id.as_str()));
        let wanted = self.wanted_side(cx);
        hosted == self.session_id.as_deref().zip(wanted.as_deref())
    }

    /// Host what the store records beside the thread, once the update
    /// asking is over: the shell and a pane may be mid-update when a
    /// thread is bound, and closing the agent asks the shell whether a
    /// pane shows it.
    pub(super) fn request_side_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.side_of.is_some() || self.side.sync_pending || self.side_in_sync(cx) {
            return;
        }
        self.side.sync_pending = true;
        cx.defer_in(window, |this, window, cx| {
            this.side.sync_pending = false;
            this.sync_side(window, cx);
        });
    }

    /// Host the agent the store records beside the thread, and only it. The
    /// one place an agent's thread is opened and closed for its lead, and
    /// its timeline leased and released: opened (unless already open) and
    /// leased as it comes; closed (unless the view did not open it, or a
    /// pane shows it now) as it goes, its lease kept until it has left.
    fn sync_side(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.side_in_sync(cx) {
            return;
        }
        let wanted = self.wanted_side(cx);
        let store = self.workspace_store.clone();
        if let Some(host) = self.side.host.take() {
            close_side_thread(&store, &host, cx);
            // Out of the compact layout, and beside the same thread, it
            // leaves where it was drawn; otherwise there is nothing for it
            // to leave from.
            let stays = !self.window_state.read(cx).compact
                && Some(host.lead.as_str()) == self.session_id.as_deref();
            if stays {
                if let Some(earlier) = self.side.leaving.replace(host) {
                    release_side_host(&store, earlier, cx);
                }
            } else {
                release_side_host(&store, host, cx);
            }
        }
        if let (Some(id), Some(lead)) = (wanted, self.session_id.clone()) {
            let host = match self.side.leaving.take() {
                Some(host) if host.session_id == id && host.lead == lead => Some(host),
                other => {
                    self.side.leaving = other;
                    None
                }
            };
            let mut host = host.unwrap_or_else(|| {
                let (window_state, me) = (self.window_state.clone(), cx.entity().downgrade());
                let chat = cx.new(|cx| {
                    ChatView::new_side(
                        store.clone(),
                        window_state,
                        me,
                        lead.clone(),
                        id.clone(),
                        window,
                        cx,
                    )
                });
                store.update(cx, |store, _| store.lease_timeline(&id));
                SideHost {
                    session_id: id.clone(),
                    lead: lead.clone(),
                    chat,
                    opened: false,
                }
            });
            host.opened = store.update(cx, |store, _| {
                let opens = !store.is_thread_open(&id);
                if opens {
                    store.open_thread(&id);
                }
                opens
            });
            self.side.host = Some(host);
        }
        cx.notify();
    }

    /// The view goes: so does the agent it hosts, and the one leaving.
    pub(super) fn release_side(&mut self, cx: &mut App) {
        let store = self.workspace_store.clone();
        if let Some(host) = self.side.host.take() {
            close_side_thread(&store, &host, cx);
            release_side_host(&store, host, cx);
        }
        if let Some(host) = self.side.leaving.take() {
            release_side_host(&store, host, cx);
        }
    }

    /// Show `id` beside the thread, with the keyboard.
    pub(crate) fn open_side(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lead) = self.session_id.clone() else {
            return;
        };
        self.workspace_store
            .update(cx, |store, cx| store.set_side_thread(&lead, Some(id), cx));
        self.sync_side(window, cx);
        // The view's own focus, which it hands on to its composer: drawn from
        // the first frame, where the composer is not until the thread loads.
        if let Some(host) = &self.side.host {
            let focus = host.chat.read(cx).focus_handle.clone();
            focus.focus(window, cx);
        }
    }

    /// Close the agent beside the thread; the keyboard it had comes back to
    /// the thread's composer.
    fn close_side(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(lead) = self.session_id.clone() else {
            return;
        };
        let had_keyboard = self
            .side
            .host
            .as_ref()
            .is_some_and(|host| host.chat.read(cx).focus_handle.contains_focused(window, cx));
        self.workspace_store
            .update(cx, |store, cx| store.set_side_thread(&lead, None, cx));
        self.sync_side(window, cx);
        if had_keyboard {
            self.focus_composer(window, cx);
        }
    }

    /// `main`, the thread's timeline, deliveries and composer in a body
    /// `width` wide, with the agent beside it: in a split with it where
    /// the body holds three of the agent's narrowest, over it otherwise.
    /// One split whether or not an agent shows, so `main` keeps its place
    /// in the tree as one comes and goes.
    pub(super) fn render_with_side(
        &mut self,
        main: AnyElement,
        width: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.side_of.is_some() {
            return main;
        }
        let compact = self.window_state.read(cx).compact;
        let mode = SideMode::of(width);
        let open = self.side.host.is_some() && !compact;
        if open {
            self.side.slot = mode;
        }
        let largest = (width - px(super::HEADER_COMPACT_MIN_WIDTH)).max(px(SIDE_MIN_WIDTH));
        let side_width = (width * self.side.share.get()).clamp(px(SIDE_MIN_WIDTH), largest);
        let frame = self.side.glide.sample(
            &[
                Panel::flex(true),
                Panel::sized(side_width, open && mode == SideMode::Split),
            ],
            !compact,
            &self.side.split,
            window,
            cx,
        );
        // gpui-base has no slide-over for one surface replacing another; the
        // agent comes in over the lead as the pull request page comes in
        // over its list (`PullRequestsPanel::render`).
        let over = self.motion.appear(
            "side-overlay",
            open && mode == SideMode::Overlay,
            SLIDE,
            window,
            cx,
        );
        let swap = self.side.host.as_ref().map_or(1., |host| {
            self.motion
                .entered("side-swap", host.session_id.clone(), SWAP, window, cx)
        });
        let settled = match self.side.host {
            Some(_) => swap >= 1.,
            None => !frame.mounted(1) && over <= 0.,
        };
        if (settled || compact)
            && let Some(left) = self.side.leaving.take()
        {
            let store = self.workspace_store.clone();
            cx.defer(move |cx| release_side_host(&store, left, cx));
        }

        // Drawn once a frame, in the one place it was last put; the other
        // place, still sliding or shrinking, holds an empty surface.
        let slot = self.side.slot;
        if let Some(host) = &self.side.host {
            host.chat.update(cx, |chat, _| {
                if let Some(of) = &mut chat.side_of {
                    of.mode = slot;
                }
            });
        }
        let mut live = Some(self.render_side_surface(swap, cx));
        let mut take_live = |here: SideMode| {
            if slot == here { live.take() } else { None }
                .unwrap_or_else(|| div().size_full().into_any_element())
        };
        let split_side = frame.mounted(1).then(|| take_live(SideMode::Split));
        // Drawn from the frame it is asked for, at nothing, so the keyboard
        // given to it has somewhere to be.
        let overlay = (over > 0. || (open && mode == SideMode::Overlay))
            .then(|| take_live(SideMode::Overlay));

        let share = self.side.share.clone();
        let group = gpui_base::h_resizable("chat-side-split")
            .with_state(&self.side.split)
            .on_resize(move |state, _, cx| {
                if let [lead, side] = state.read(cx).sizes()[..] {
                    let total = lead + side;
                    if total > px(0.) {
                        share.set(side / total);
                    }
                }
            })
            .child(
                gpui_base::resizable_panel()
                    .size_range(frame.range(0, gpui_base::PANEL_MIN_SIZE..Pixels::MAX))
                    .child(frame.hold(0, main)),
            )
            // Not `children`: the group's replaces the panels added before.
            .when_some(split_side, |group, side| {
                group.child(
                    gpui_base::resizable_panel()
                        .size(side_width)
                        .size_range(frame.range(1, px(SIDE_MIN_WIDTH)..largest))
                        .child(frame.hold(1, side)),
                )
            });
        let leaving = !(open && mode == SideMode::Overlay);
        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(
                // The lead moves a quarter left under the agent coming over
                // it, and is not drawn once covered.
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(-0.25 * over))
                    .right(relative(0.25 * over))
                    .when(over >= 1., |lead| lead.invisible())
                    .child(group),
            )
            .when_some(overlay, |body, overlay| {
                body.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(relative(1. - over))
                        .right(relative(over - 1.))
                        .opacity(if leaving { 1. } else { over })
                        .bg(crate::material::content_surface(cx))
                        .occlude()
                        .child(overlay),
                )
            })
            // Neither takes input while one slides over the other.
            .when(over > 0. && over < 1., |body| {
                body.child(div().absolute().inset_0().occlude())
            })
            .into_any_element()
    }

    /// The agent drawn beside the thread, the one it replaced fading out
    /// under it, where Escape closes it.
    fn render_side_surface(&self, swap: f32, cx: &mut Context<Self>) -> AnyElement {
        let current = self.side.host.as_ref().or(self.side.leaving.as_ref());
        let under = self
            .side
            .host
            .as_ref()
            .and(self.side.leaving.as_ref())
            .filter(|_| swap < 1.);
        div()
            .size_full()
            .relative()
            .key_context(SIDE_CONTEXT)
            .on_action(cx.listener(|this, _: &CloseSide, window, cx| this.close_side(window, cx)))
            .children(under.map(|under| div().absolute().inset_0().child(under.chat.clone())))
            .children(current.map(|current| {
                div()
                    .absolute()
                    .inset_0()
                    .when(under.is_some(), |layer| layer.opacity(swap))
                    .child(current.chat.clone())
            }))
            .into_any_element()
    }

    /// An agent's own header beside its lead: Back at its start while it
    /// covers the lead, its state, title and provider, and Close at its end
    /// while it sits beside the lead.
    pub(super) fn render_side_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let of = self.side_of.as_ref()?;
        let session = self.session_id.clone()?;
        let (heading, meta_title, meta_detail, lead_title) = {
            let store = self.workspace_store.read(cx);
            let heading =
                crate::agents_panel::Agents::of(store, &of.lead_session).heading(&session);
            let meta = store.thread_meta(&session);
            let detail = meta.map(|meta| {
                [Some(meta.provider.display_name()), meta.model.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ")
            });
            (
                heading,
                meta.map(|meta| meta.title.clone()),
                detail,
                store
                    .thread_meta(&of.lead_session)
                    .map(|meta| meta.title.clone())
                    .unwrap_or_default(),
            )
        };
        let title = heading
            .as_ref()
            .map(|heading| heading.title.clone())
            .or(meta_title)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| crate::tr!("agents.untitled").into_owned());
        let detail = heading
            .as_ref()
            .map(|heading| heading.detail.clone())
            .or(meta_detail)
            .unwrap_or_default();
        let execution = heading.as_ref().and_then(|heading| heading.execution);
        let key = heading.map_or_else(|| session.clone(), |heading| heading.key);

        let close = {
            let lead = of.lead.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = lead.update(cx, |lead, cx| lead.close_side(window, cx));
            }
        };
        // Back while it covers the lead, Close while it sits beside it: one
        // folds out as the other folds in when the body crosses between.
        let back = self
            .motion
            .shown(
                "side-back",
                (of.mode == SideMode::Overlay).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                let name: SharedString = crate::tr!("agents.back_to_parent", title = lead_title)
                    .into_owned()
                    .into();
                shown.fold_across(
                    Button::new("side-back")
                        .motion(&self.motion)
                        .debug_selector(|| "side-back".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::ArrowLeft)
                        .aria_label(name.clone())
                        .tooltip(name)
                        .on_click(close.clone()),
                )
            });
        let close = self
            .motion
            .shown(
                "side-close",
                (of.mode == SideMode::Split).then_some(()),
                SHOW,
                window,
                cx,
            )
            .map(|shown| {
                let name: SharedString = crate::tr!("agents.close_side").into_owned().into();
                shown.fold_across(
                    Button::new("side-close")
                        .motion(&self.motion)
                        .debug_selector(|| "side-close".into())
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .aria_label(name.clone())
                        .tooltip(name)
                        .on_click(close),
                )
            });
        let status = execution.map(|execution| {
            self.motion.cross_fade_part(
                "side-status",
                execution,
                CHANGE,
                window,
                cx,
                move |execution, _, cx| {
                    div()
                        .text_color(crate::agents_panel::execution_color(*execution, cx))
                        .child(crate::agents_panel::execution_label(*execution))
                        .into_any_element()
                },
            )
        });
        let muted = cx.theme().muted_foreground;
        Some(
            crate::material::bar("chat-side-header", crate::material::Bar::Toolbar, false, cx)
                .debug_selector(|| "chat-side-header".into())
                .items_center()
                .children(back)
                .child(div().flex_none().child(crate::agents_panel::status_mark(
                    &key, execution, window, cx,
                )))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_body(false, cx)
                        .child(crate::widgets::fade_text(title)),
                )
                .child(
                    h_flex()
                        .min_w_0()
                        .max_w(relative(0.5))
                        .gap_1()
                        .items_center()
                        .text_secondary(false, cx)
                        .text_color(muted)
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .child(crate::widgets::fade_text(detail)),
                        )
                        .when_some(status, |line, status| {
                            line.child(div().flex_none().child("·"))
                                .child(div().flex_none().child(status))
                        }),
                )
                .children(close)
                .into_any_element(),
        )
    }
}

/// Close `host`'s thread if the host opened it and no pane shows it now.
fn close_side_thread(store: &Entity<WorkspaceStore>, host: &SideHost, cx: &mut App) {
    if host.opened
        && store.read(cx).is_thread_open(&host.session_id)
        && !crate::shell::shown_in_pane(&host.session_id, cx)
    {
        store.update(cx, |store, cx| store.close_thread(&host.session_id, cx));
    }
}

/// Let go of `host`'s lease; its chat goes with it.
fn release_side_host(store: &Entity<WorkspaceStore>, host: SideHost, cx: &mut App) {
    store.update(cx, |store, cx| store.release_timeline(&host.session_id, cx));
}
