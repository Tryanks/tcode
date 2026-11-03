//! An agent opened from its lead, shown inside the lead's own chat: on a
//! card in the side rail's place where the body has room for it beside the
//! lead's column, over the lead's timeline and composer otherwise. The
//! lead's header stays; the agent has a header of its own. The store records
//! which agent is beside which lead ([`WorkspaceStore::side_thread_for`]);
//! the lead's chat hosts it, holding the agent's thread open and its
//! timeline leased while it draws it.

use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, Context, DragMoveEvent, Entity, EntityId,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_base::{ResizeHandleContext, ResizeHandleState};

use super::ChatView;
use crate::icon::IconName;
use crate::material::CHAT_RAIL_GAP;
use crate::motion::Colour;
use crate::motion::owned::Owned;
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::widgets::button::{Button, ButtonVariants as _};

gpui::actions!(tcode_chat, [CloseSide]);

/// The key context of an agent shown beside its lead, where Escape closes it.
pub(crate) const SIDE_CONTEXT: &str = "ChatSide";

/// The share of the body an agent's card beside its lead takes until its
/// edge is dragged.
const SIDE_SHARE: f32 = 1. / 3.;

/// How long the agent takes to slide over the lead, or off it.
const SLIDE: Duration = Duration::from_millis(250);

/// How long one agent beside the lead takes to cross-fade into another.
const SWAP: Duration = Duration::from_millis(200);

/// How long what the side header shows only some of the time takes to come
/// in or go, and the agent's card to come into the rail's place or leave it.
const SHOW: Duration = Duration::from_millis(200);

/// The grip of the card's edge: the sheet grabber's length, standing.
const GRIP_LENGTH: f32 = 36.;
const GRIP_WIDTH: f32 = 4.;

/// Where an agent beside its lead is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SideMode {
    /// On a card in the side rail's place, right of the lead's column.
    Slot,
    /// Over the lead's timeline and composer, under the lead's header.
    Overlay,
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
    /// The agent last shown, drawn until it has slid or faded away.
    leaving: Option<SideHost>,
    /// The share of the body the agent's card takes, as its edge was last
    /// dragged; kept by the view only, never saved.
    share: f32,
    /// Where the agent drawn was last drawn: a leaving one stays there.
    slot: SideMode,
    sync_pending: bool,
}

impl Side {
    pub(super) fn new() -> Self {
        Self {
            host: None,
            leaving: None,
            share: SIDE_SHARE,
            slot: SideMode::Slot,
            sync_pending: false,
        }
    }
}

/// A drag of the edge of the agent's card beside the lead `0`, which only
/// that lead's chat follows.
#[derive(Clone, Copy)]
struct SideEdgeDrag(EntityId);

impl Render for SideEdgeDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// One frame of the agent beside the thread: see [`ChatView::render_side`].
#[derive(Default)]
pub(super) struct SideFrame {
    /// Its card in the rail's place, while it is there or fading out of it.
    pub(super) card: Option<AnyElement>,
    /// It over the lead, while it is there or sliding on or off.
    overlay: Option<AnyElement>,
    /// How far over the lead it is.
    over: f32,
    /// Whether what is over the lead is leaving it.
    leaving: bool,
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
            mode: SideMode::Slot,
        });
        chat.set_session(Some(session_id), cx);
        chat
    }

    /// Whether the view is an agent's shown beside its lead.
    pub(super) fn is_side(&self) -> bool {
        self.side_of.is_some()
    }

    /// Whether the view is an agent drawn on the card beside its lead, which
    /// paints the surface under it.
    pub(super) fn on_side_card(&self) -> bool {
        self.side_of
            .as_ref()
            .is_some_and(|of| of.mode == SideMode::Slot)
    }

    /// The agent shown beside the thread.
    pub(crate) fn side_session_id(&self) -> Option<&str> {
        self.side.host.as_ref().map(|host| host.session_id.as_str())
    }

    /// The share of the body the agent beside the thread takes in the
    /// rail's place, if one is shown: never in the compact layout.
    pub(super) fn side_share(&self, cx: &App) -> Option<f32> {
        (self.side.host.is_some() && !self.window_state.read(cx).compact).then_some(self.side.share)
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

    /// This frame's agent beside the thread, laid out by the body's layout
    /// (`self.body`): its card in the rail's place, for the timeline to
    /// hold, and what slides over the lead, for [`Self::cover_with_side`].
    pub(super) fn render_side(&mut self, window: &mut Window, cx: &mut Context<Self>) -> SideFrame {
        if self.side_of.is_some() {
            return SideFrame::default();
        }
        let compact = self.window_state.read(cx).compact;
        let mode = if self.body.side() {
            SideMode::Slot
        } else {
            SideMode::Overlay
        };
        let open = self.side.host.is_some() && !compact;
        if open {
            self.side.slot = mode;
        }
        let presence = self.body.side_presence();
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
            None => presence <= 0. && over <= 0.,
        };
        if (settled || compact)
            && let Some(left) = self.side.leaving.take()
        {
            let store = self.workspace_store.clone();
            cx.defer(move |cx| release_side_host(&store, left, cx));
        }

        // Drawn once a frame, in the one place it was last put; the other
        // place, still sliding or fading, holds an empty surface.
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
        // The card stays while it fades out empty, so it never goes in the
        // frame the agent crosses over the lead.
        let card = (presence > 0.).then(|| {
            let content = take_live(SideMode::Slot);
            self.render_side_card(content, cx)
        });
        // Drawn from the frame it is asked for, at nothing, so the keyboard
        // given to it has somewhere to be.
        let overlay = (over > 0. || (open && mode == SideMode::Overlay))
            .then(|| take_live(SideMode::Overlay));
        SideFrame {
            card,
            overlay,
            over,
            leaving: !(open && mode == SideMode::Overlay),
        }
    }

    /// The agent's card in the rail's place, holding `content`, its leading
    /// edge dragged to widen or narrow it.
    fn render_side_card(&self, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let shown = self.body.side();
        // gpui-base's resize handle straddles the leading edge of what holds
        // it; held by nothing as wide, it straddles the middle of the gap
        // left of the card, which the drag measures the card's width from.
        let edge = shown.then(|| {
            let motion = self.motion.clone();
            let lead = cx.entity_id();
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-CHAT_RAIL_GAP / 2.))
                .child(
                    gpui_base::resize_handle::<SideEdgeDrag, SideEdgeDrag>(
                        "chat-side-edge",
                        gpui::Axis::Horizontal,
                    )
                    .with_appearance(std::rc::Rc::new(
                        move |handle: &ResizeHandleContext, window: &mut Window, cx: &mut App| {
                            Some(side_grip(&motion, handle, window, cx))
                        },
                    ))
                    .on_drag(SideEdgeDrag(lead), |drag, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| *drag)
                    }),
                )
        });
        super::rail::slot(self.body.side_width(), self.body.side_presence(), cx)
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<SideEdgeDrag>, _, cx| {
                    if event.drag(cx).0 != cx.entity_id() {
                        return;
                    }
                    let width =
                        event.bounds.right() - event.event.position.x - px(CHAT_RAIL_GAP / 2.);
                    let share = this.body.side_share_at(width);
                    if share != this.side.share {
                        this.side.share = share;
                        cx.notify();
                    }
                }),
            )
            .child(
                crate::material::card_raised(cx)
                    .debug_selector(|| "chat-side-card".into())
                    .p_0()
                    .gap_0()
                    .size_full()
                    .overflow_hidden()
                    .child(content),
            )
            .children(edge)
            .when(!shown, |card| card.child(crate::motion::shield()))
            .into_any_element()
    }

    /// `main`, the thread's timeline, deliveries and composer, under the
    /// agent sliding over it in `side`. Always the same elements around
    /// `main`, so it keeps its place in the tree as an agent comes and goes.
    pub(super) fn cover_with_side(
        &self,
        main: AnyElement,
        side: SideFrame,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.side_of.is_some() {
            return main;
        }
        let over = side.over;
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
                    .child(main),
            )
            .when_some(side.overlay, |body, overlay| {
                body.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(relative(1. - over))
                        .right(relative(over - 1.))
                        .opacity(if side.leaving { 1. } else { over })
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
    /// covers the lead, its state and title, and Close at its end while it
    /// sits beside the lead.
    pub(super) fn render_side_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let of = self.side_of.as_ref()?;
        let session = self.session_id.clone()?;
        let (heading, meta_title, lead_title) = {
            let store = self.workspace_store.read(cx);
            let heading =
                crate::agents_panel::Agents::of(store, &of.lead_session).heading(&session);
            (
                heading,
                store.thread_meta(&session).map(|meta| meta.title.clone()),
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
                (of.mode == SideMode::Slot).then_some(()),
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
                .children(close)
                .into_any_element(),
        )
    }
}

/// What the edge of the agent's card paints in the gap left of the card: no
/// line, for the gap is the divider, and a short pill centred in it, fading
/// in while the pointer is on the edge and firming while it holds it.
///
/// gpui-base's handle paints a hairline in the middle of its band; its
/// appearance is the caller's to replace, and this is laid out in that
/// one-pixel middle, centred on its leading side.
fn side_grip(
    motion: &Owned,
    handle: &ResizeHandleContext,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let fill = match handle.state() {
        ResizeHandleState::Idle => muted.opacity(0.),
        ResizeHandleState::Hovered => muted.opacity(0.3),
        ResizeHandleState::Pressed | ResizeHandleState::Dragging => muted.opacity(0.6),
    };
    let fill = motion.transition(
        "side-grip",
        Colour(fill),
        crate::material::HOVER,
        window,
        cx,
    );
    div()
        .flex_none()
        .h_full()
        .w(px(GRIP_WIDTH))
        .ml(px(-GRIP_WIDTH / 2.))
        .flex()
        .items_center()
        .child(
            div()
                .debug_selector(|| "chat-side-grip".into())
                .w_full()
                .h(px(GRIP_LENGTH))
                .rounded_full()
                .bg(fill.0),
        )
        .into_any_element()
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
