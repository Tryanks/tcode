//! The thread's side rail: cards stacked in the room a wide chat leaves
//! right of its content column, each showing what it holds.

mod agents;
mod changes;
pub(super) mod commands;
mod goal;
mod plan;
mod pull_requests;
pub(super) mod screen;
mod stack;
mod thread;

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, InteractiveElement as _, IntoElement as _,
    ParentElement as _, Pixels, Render, SharedString, Styled as _, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::StyledExt as _;
use tcode_core::ui::RightTab;

use super::{ChatView, TimelineView};
use crate::icon::{Icon, IconName};
use crate::material::{
    CARD_INSET, CHAT_CONTENT_MAX_WIDTH, CHAT_CONTENT_MIN_PADDING, CHAT_RAIL_EDGE, CHAT_RAIL_GAP,
    CHAT_RAIL_NARROWEST, CHAT_RAIL_WIDEST, CHAT_SIDE_GRIP, PANEL_CONTENT_MIN_WIDTH,
};
use crate::scroll::ScrollableElement as _;
use crate::sizing::Sizable as _;
use crate::store::WorkspaceStore;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;
use crate::window_state::WindowState;

/// How long the rail, or a card in it, takes to come in or go.
const SHOW: Duration = Duration::from_millis(200);

/// How far right of its place what takes the slot comes in from and leaves
/// to.
const SLIDE: f32 = 16.;

/// How the thread's body sits at its width: what takes the slot right of its
/// content column (the side rail, or an agent shown beside the thread on a
/// card), how wide that is, and how far the content column moves left to
/// make room for it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct BodyLayout {
    /// The body's width.
    body: Pixels,
    /// See [`Self::rail`].
    rail: bool,
    /// See [`Self::width`].
    width: Pixels,
    /// The right inset that keeps the column clear of the rail all the way in.
    reach: Pixels,
    /// How far in the rail is, from absent to at rest.
    presence: f32,
    /// See [`Self::side`].
    side: bool,
    /// See [`Self::side_width`].
    side_width: Pixels,
    /// The right inset that keeps the column clear of the agent's card.
    side_reach: Pixels,
    /// How far in the agent's card is.
    side_presence: f32,
}

impl BodyLayout {
    /// The layout of a body `width` wide, below its header, with an agent
    /// beside the thread at `side_share` of it, if one is. Whatever takes
    /// the slot right of the column sits in the gutter right of the centred
    /// column where it fits there, and the column never moves for it;
    /// narrower, the column moves left just as far as it needs, and never so
    /// far that it narrows; narrower still, or compact, nothing takes the
    /// slot. The rail fills its gutter up to its widest; the agent's card
    /// takes its share, within its narrowest and what the column leaves, and
    /// the rail gives way to it. The card holds its edge's grip in the slot
    /// with it ([`CHAT_SIDE_GRIP`]), so the column clears that too.
    pub(super) fn of(
        width: Pixels,
        has_cards: bool,
        side_share: Option<f32>,
        compact: bool,
    ) -> Self {
        let body = f32::from(width);
        let room = (body - CHAT_CONTENT_MAX_WIDTH) / 2. - CHAT_RAIL_GAP - CHAT_RAIL_EDGE;
        let rail_width = room.clamp(CHAT_RAIL_NARROWEST, CHAT_RAIL_WIDEST);
        let widest_side = side_widest(body);
        let side_width = side_share.map_or(PANEL_CONTENT_MIN_WIDTH, |share| {
            (body * share).min(widest_side).max(PANEL_CONTENT_MIN_WIDTH)
        });
        let side = !compact
            && side_share.is_some()
            && fits(
                body,
                PANEL_CONTENT_MIN_WIDTH + CHAT_SIDE_GRIP,
                CHAT_CONTENT_MAX_WIDTH,
            );
        Self {
            body: width,
            rail: !compact
                && fits(body, CHAT_RAIL_NARROWEST, RAIL_COLUMN_MIN)
                && has_cards
                && !side,
            width: px(rail_width),
            reach: px(if compact {
                0.
            } else {
                reach(body, rail_width, RAIL_COLUMN_MIN)
            }),
            presence: 0.,
            side,
            side_width: px(side_width),
            side_reach: px(if compact {
                0.
            } else {
                reach(body, side_width + CHAT_SIDE_GRIP, CHAT_CONTENT_MAX_WIDTH)
            }),
            side_presence: 0.,
        }
    }

    /// Whether the rail shows: it fits, has a card to show, and no agent
    /// beside the thread takes its slot.
    pub(super) fn rail(&self) -> bool {
        self.rail
    }

    /// How wide the rail is, which its cards lay out their bodies at.
    pub(super) fn width(&self) -> Pixels {
        self.width
    }

    /// Whether an agent beside the thread takes the rail's slot: one is,
    /// and its card fits there at its narrowest. One that does not fit
    /// covers the thread instead.
    pub(super) fn side(&self) -> bool {
        self.side
    }

    /// How wide the agent's card in the slot is.
    pub(super) fn side_width(&self) -> Pixels {
        self.side_width
    }

    /// How far in the agent's card is, from absent to at rest.
    pub(super) fn side_presence(&self) -> f32 {
        self.side_presence
    }

    /// The share of the body an agent's card dragged to `width` takes,
    /// within the widths it can have.
    pub(super) fn side_share_at(&self, width: Pixels) -> f32 {
        let body = f32::from(self.body);
        if body <= 0. {
            return 0.;
        }
        f32::from(width)
            .min(side_widest(body))
            .max(PANEL_CONTENT_MIN_WIDTH)
            / body
    }

    /// The layout with its rail `presence` and the agent's card
    /// `side_presence` of the way in.
    pub(super) fn at(self, presence: f32, side_presence: f32) -> Self {
        Self {
            presence,
            side_presence,
            ..self
        }
    }

    /// The right inset the rows, the deliveries, the composer and the jump
    /// to the latest row add to their own, so the column centres left of
    /// what takes the slot; while one gives way to the other, it moves from
    /// one's inset to the other's. Applied as padding only: the list and the
    /// column keep their widths, so no row is measured again as the slot
    /// changes hands.
    pub(super) fn shift(&self) -> Pixels {
        self.blend(self.reach, self.side_reach)
    }

    /// `rail` as far as the rail is in and `side` as far as the agent's
    /// card is; while one gives way to the other, from one to the other.
    fn blend(&self, rail: Pixels, side: Pixels) -> Pixels {
        let rail = rail * self.presence;
        let side = side * self.side_presence;
        let both = self.presence + self.side_presence;
        if both > 1. {
            (rail + side) / both
        } else {
            rail + side
        }
    }
}

/// What the slot takes from the body for something `width` wide in it: its
/// gap from the column and its edge.
const fn span(width: f32) -> f32 {
    width + CHAT_RAIL_GAP + CHAT_RAIL_EDGE
}

/// The narrowest the column gets for the rail beside it: what a new window
/// leaves it with the sidebar at its narrowest, so the cards show there. A
/// wider window keeps the widest column.
const RAIL_COLUMN_MIN: f32 = crate::shell::DEFAULT_WINDOW_WIDTH
    - crate::shell::SIDEBAR_MIN_WIDTH
    - CHAT_CONTENT_MIN_PADDING
    - span(CHAT_RAIL_NARROWEST);

/// Whether something `width` wide fits in the slot of a body `body` wide,
/// beside a column `column` wide moved left as far as the page inset.
fn fits(body: f32, width: f32, column: f32) -> bool {
    body >= column + CHAT_CONTENT_MIN_PADDING + span(width)
}

/// How far the column moves left in a body `body` wide for something
/// `width` wide in the slot: nothing while it fits in the gutter right of
/// the centred column, and never so far that the column narrows below
/// `column`.
fn reach(body: f32, width: f32, column: f32) -> f32 {
    (CHAT_CONTENT_MAX_WIDTH + 2. * span(width) - body)
        .clamp(0., span(width) - CHAT_CONTENT_MIN_PADDING)
        .min((body - column - 2. * CHAT_CONTENT_MIN_PADDING).max(0.))
}

/// The widest an agent's card in the slot of a body `body` wide is: as wide
/// as the widest column leaves.
fn side_widest(body: f32) -> f32 {
    body - CHAT_CONTENT_MAX_WIDTH
        - CHAT_CONTENT_MIN_PADDING
        - CHAT_RAIL_GAP
        - CHAT_RAIL_EDGE
        - CHAT_SIDE_GRIP
}

/// The slot right of the content column that the rail or an agent's card
/// takes, `width` wide and `presence` of the way in, in a [`Slots`]: from
/// the header's bottom edge down to the bottom of the composer's card. It
/// fades and slides in from the right, and out to it.
pub(super) fn slot(width: Pixels, presence: f32) -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right(px(CHAT_RAIL_EDGE - SLIDE * (1. - presence)))
        .w(width)
        .opacity(presence)
}

/// What takes the slot, over the whole of the thread's body under its
/// header (`slots`), ending where the composer's card does: `below_card`
/// above the body's bottom, which the composer measures as it is laid out
/// ([`crate::composer::Composer::below_card`]). The column beside the slot
/// keeps clear of it ([`BodyLayout::shift`]), the composer's included.
///
/// Taffy cannot place one box by another's descendant, so this is laid out
/// after the body, from the composer's measurement of this frame; it must
/// come after the composer among the body's children.
pub(super) struct Slots {
    pub(super) below_card: std::rc::Rc<std::cell::Cell<Pixels>>,
    pub(super) slots: Vec<AnyElement>,
}

impl gpui::IntoElement for Slots {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for Slots {
    type RequestLayoutState = ();
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        let mut style = gpui::Style {
            position: gpui::Position::Absolute,
            ..Default::default()
        };
        style.inset = gpui::Edges::all(px(0.).into());
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        if self.slots.is_empty() {
            return None;
        }
        let height = (bounds.size.height - self.below_card.get()).max(px(0.));
        let mut slots = div()
            .relative()
            .w(bounds.size.width)
            .h(height)
            .children(std::mem::take(&mut self.slots))
            .into_any_element();
        slots.layout_as_root(
            gpui::size(
                gpui::AvailableSpace::Definite(bounds.size.width),
                gpui::AvailableSpace::Definite(height),
            ),
            window,
            cx,
        );
        slots.prepaint_at(bounds.origin, window, cx);
        Some(slots)
    }

    fn paint(
        &mut self,
        _: Option<&gpui::GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        _: gpui::Bounds<Pixels>,
        _: &mut (),
        slots: &mut Option<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(slots) = slots {
            slots.paint(window, cx);
        }
    }
}

/// A card the rail can show, in the order it shows them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RailCard {
    Goal,
    Plan,
    Agents,
    Commands,
    Screen,
    Changes,
    PullRequests,
    Thread,
}

impl RailCard {
    const ALL: [Self; 8] = [
        Self::Goal,
        Self::Plan,
        Self::Agents,
        Self::Commands,
        Self::Screen,
        Self::Changes,
        Self::PullRequests,
        Self::Thread,
    ];

    /// Its key among the rail's cards and in the view's expansion state.
    fn key(self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Plan => "plan",
            Self::Agents => "agents",
            Self::Commands => "commands",
            Self::Screen => "screen",
            Self::Changes => "changes",
            Self::PullRequests => "pull-requests",
            Self::Thread => "thread",
        }
    }

    fn title(self) -> Cow<'static, str> {
        match self {
            Self::Goal => crate::tr!("chat.rail_goal"),
            Self::Plan => crate::tr!("chat.rail_plan"),
            Self::Agents => crate::tr!("chat.rail_agents"),
            Self::Commands => crate::tr!("chat.rail_commands"),
            Self::Screen => crate::tr!("chat.rail_screen"),
            Self::Changes => crate::tr!("chat.rail_changes"),
            Self::PullRequests => crate::tr!("chat.rail_pull_requests"),
            Self::Thread => crate::tr!("chat.rail_thread"),
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Goal => IconName::CircleDot,
            Self::Plan => IconName::Map,
            Self::Agents => IconName::Bot,
            Self::Commands => IconName::SquareTerminal,
            Self::Screen => IconName::Monitor,
            Self::Changes => IconName::Files,
            Self::PullRequests => IconName::GitPullRequest,
            Self::Thread => IconName::Info,
        }
    }

    /// The right panel's tab that shows what the card does, which the card
    /// gives way to while it is open.
    fn tab(self) -> Option<RightTab> {
        match self {
            Self::Goal | Self::Commands | Self::Screen | Self::Thread => None,
            Self::Plan => Some(RightTab::Plan),
            Self::Agents => Some(RightTab::Agents),
            Self::Changes => Some(RightTab::Diff),
            Self::PullRequests => Some(RightTab::PullRequests),
        }
    }

    fn present(self, rail: &RailView, cx: &App) -> bool {
        match self {
            Self::Goal => goal::present(rail, cx),
            Self::Plan => plan::present(rail, cx),
            Self::Agents => agents::present(rail, cx),
            Self::Commands => commands::present(rail, cx),
            Self::Screen => screen::present(rail, cx),
            Self::Changes => changes::present(rail, cx),
            Self::PullRequests => pull_requests::present(rail, cx),
            Self::Thread => thread::present(rail, cx),
        }
    }

    /// What its header says beside its title, where that is a fact the
    /// top of its body does not already show.
    fn summary(self, rail: &RailView, cx: &App) -> Option<SharedString> {
        match self {
            Self::Goal => goal::summary(rail, cx),
            Self::Plan => plan::summary(rail, cx),
            Self::Agents => agents::summary(rail, cx),
            Self::Commands => commands::summary(rail, cx),
            Self::Changes => changes::summary(rail, cx),
            Self::Screen | Self::PullRequests | Self::Thread => None,
        }
    }

    /// Whether its body is a list that scrolls itself inside the card, with
    /// the card's inset, rather than content the card scrolls.
    fn lists_itself(self) -> bool {
        self == Self::Agents
    }

    /// Whether its body is content of a height of its own (a picture and
    /// its caption, a clamped objective and verdict) rather than rows that
    /// grow with what the thread holds: the rail gives it all of that
    /// height while the other cards' bodies can scroll to make room.
    fn rigid(self) -> bool {
        matches!(self, Self::Goal | Self::Screen)
    }

    /// The rows of its body, when they never scroll: in less height than
    /// they all take the card shows as many as fit and counts the rest. The
    /// body holds the card's inset itself.
    fn fit(self, rail: &RailView, cx: &App) -> Option<stack::Fit> {
        match self {
            Self::Commands => Some(commands::fit(rail, cx)),
            Self::Goal
            | Self::Plan
            | Self::Agents
            | Self::Screen
            | Self::Changes
            | Self::PullRequests
            | Self::Thread => None,
        }
    }

    /// What its header offers at its end: the stop for all of the thread's
    /// background work, on the Commands card, or on the Agents card while
    /// the thread has no commands to list.
    fn action(self, rail: &RailView, cx: &mut Context<RailView>) -> Option<AnyElement> {
        let stops_all = match self {
            Self::Commands => true,
            Self::Agents => !commands::present(rail, cx),
            _ => false,
        };
        if !stops_all {
            return None;
        }
        let background = rail.background(cx)?;
        let place = rail.background_place(cx)?;
        super::background::stop_all(&background, &place, cx)
    }

    fn body(
        self,
        rail: &RailView,
        window: &mut Window,
        cx: &mut Context<RailView>,
    ) -> Option<AnyElement> {
        match self {
            Self::Goal => goal::body(rail, window, cx),
            Self::Plan => plan::body(rail, window, cx),
            Self::Agents => agents::body(rail, window, cx),
            Self::Commands => commands::body(rail, window, cx),
            Self::Screen => screen::body(rail, window, cx),
            Self::Changes => changes::body(rail, window, cx),
            Self::PullRequests => pull_requests::body(rail, window, cx),
            Self::Thread => thread::body(rail, window, cx),
        }
    }
}

/// The rail, drawn by a view of its own in the chat's body.
///
/// [`Slots`] and [`stack::RailStack`] lay the cards out as layouts of their
/// own, in prepaint. gpui-fast builds a nested view again alone only where
/// its layout hangs under the layout of the view around it, so a view drawn
/// in a card (a running command's spinner, which turns every frame, and its
/// clock, which ticks every second) builds the nearest view around those
/// layouts again with it. That is this view, not the chat with its header,
/// timeline and composer.
pub(super) struct RailView {
    workspace_store: Entity<WorkspaceStore>,
    window_state: Entity<WindowState>,
    session_id: Option<String>,
    chat: WeakEntity<ChatView>,
    /// The lead, while the chat is an agent's shown beside it.
    lead: Option<WeakEntity<ChatView>>,
    timeline: Entity<TimelineView>,
    /// The motion of what the rail draws, in the chat's scene.
    motion: crate::motion::owned::Owned,
    /// Where the rail's agents were drawn, for an agent opened from one to
    /// grow out of it. The chat resolves them with its own.
    pub(super) origins: crate::motion::expand::Origins,
    /// How the chat's body sits this frame and the cards it shows, as the
    /// chat last handed them.
    layout: RailLayout,
    /// Where the composer's card ends: see [`Slots`].
    below_card: Rc<Cell<Pixels>>,
    /// Open/closed keys of the rail's collapsibles.
    expanded: HashSet<String>,
    /// The screenshots the Screen card steps through.
    screen: screen::Screenshots,
    /// The commands the Commands card lists.
    commands: commands::Commands,
    /// The piece of background work the timeline pointed out, by its call,
    /// which the Commands or Agents card marks.
    background_highlight: Option<String>,
}

/// How the chat's body sits this frame ([`BodyLayout`]), and the cards
/// the rail shows in it.
#[derive(Clone, Default, PartialEq)]
pub(super) struct RailLayout {
    pub(super) body: BodyLayout,
    pub(super) cards: Vec<RailCard>,
}

impl RailView {
    pub(super) fn new(
        workspace_store: Entity<WorkspaceStore>,
        window_state: Entity<WindowState>,
        session_id: Option<String>,
        scene: &crate::motion::Epoch,
        chat: WeakEntity<ChatView>,
        timeline: Entity<TimelineView>,
        below_card: Rc<Cell<Pixels>>,
    ) -> Self {
        Self {
            workspace_store,
            window_state,
            session_id,
            chat,
            lead: None,
            timeline,
            motion: crate::motion::owned::Owned::in_scene(scene),
            origins: crate::motion::expand::Origins::in_scene(scene),
            layout: RailLayout::default(),
            below_card,
            expanded: HashSet::new(),
            screen: Default::default(),
            commands: Default::default(),
            background_highlight: None,
        }
    }

    /// Show `session_id`'s thread, or none: its collapsibles start closed.
    pub(super) fn set_session(&mut self, session_id: Option<String>, cx: &mut Context<Self>) {
        self.session_id = session_id;
        self.expanded.clear();
        changes::forget(self);
        cx.notify();
    }

    /// Mark the piece of background work the call `item_id` started.
    pub(super) fn highlight_background(&mut self, item_id: Option<String>, cx: &mut Context<Self>) {
        if self.background_highlight != item_id {
            self.background_highlight = item_id;
            cx.notify();
        }
    }

    /// The thread's background work, while its harness runs any.
    fn background(&self, cx: &App) -> Option<super::background::Background> {
        super::background::Background::of(
            self.workspace_store.read(cx),
            self.session_id.as_deref()?,
            cx,
        )
    }

    /// Where the rail draws pieces of the background work.
    fn background_place(&self, cx: &mut Context<Self>) -> Option<super::background::Place> {
        let rail = cx.entity().downgrade();
        Some(super::background::Place {
            store: self.workspace_store.clone(),
            session_id: self.session_id.clone()?,
            chat: self.chat.clone(),
            key: "rail",
            highlight: self.background_highlight.clone(),
            touch: false,
            leave: None,
            redraw: Rc::new(move |cx| {
                let _ = rail.update(cx, |_, cx| cx.notify());
            }),
        })
    }

    /// The rail is an agent's shown beside `lead`.
    pub(super) fn set_lead(&mut self, lead: WeakEntity<ChatView>) {
        self.lead = Some(lead);
    }

    /// How the chat lays its body out this frame, and the cards it shows.
    pub(super) fn set_layout(&mut self, layout: RailLayout) {
        self.layout = layout;
    }

    /// List again what the Screen and Commands cards show, from the
    /// thread's items and background work as they are now.
    pub(super) fn sync(&mut self, cx: &App) {
        changes::forget(self);
        let (screen, commands) = (&mut self.screen, &mut self.commands);
        let store = self.workspace_store.read(cx);
        let tasks = store
            .session_status_for(self.session_id.as_deref(), cx)
            .map(|status| status.background_tasks.as_slice())
            .unwrap_or_default();
        let listed = store.with_entries_for(
            self.session_id.as_deref(),
            |timeline| {
                screen.sync(&timeline.entries);
                commands.sync(timeline, tasks);
            },
            cx,
        );
        if listed.is_none() {
            self.screen.sync(&[]);
            self.commands = Default::default();
        }
    }

    /// The chat an agent opened from the rail shows beside its thread: see
    /// [`super::side::agent_host`].
    fn agent_host(&self, cx: &App) -> Option<Entity<ChatView>> {
        super::side::agent_host(
            &self.chat,
            self.lead.as_ref(),
            self.session_id.is_some(),
            &self.window_state,
            cx,
        )
    }
}

impl Render for RailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        self.motion.sweep();
        self.origins.begin_frame();
        Slots {
            below_card: self.below_card.clone(),
            slots: self.render_rail(window, cx).into_iter().collect(),
        }
    }
}

impl RailView {
    /// The cards the rail shows for the thread now: those the host reports
    /// something for, less any whose tab the right panel shows beside it.
    pub(super) fn cards(&self, cx: &App) -> Vec<RailCard> {
        let store = self.workspace_store.read(cx);
        let panel = Some(store.panel_state_for(self.session_id.as_deref()))
            .filter(|panel| panel.right_panel_open);
        RailCard::ALL
            .into_iter()
            .filter(|card| {
                panel
                    .as_ref()
                    .is_none_or(|panel| Some(panel.right_tab) != card.tab())
                    && card.present(self, cx)
            })
            .collect()
    }

    /// The rail in the slot right of the column, sliding and fading in and
    /// out with the layout's presence, showing the cards the chat handed.
    fn render_rail(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let RailLayout { body, cards } = &self.layout;
        let presence = body.presence;
        // Laid out from the frame it starts to come in, unseen: an agent
        // closing shrinks toward its row from the frame after.
        if (presence <= 0. && !body.rail) || self.window_state.read(cx).compact {
            return None;
        }
        // A card that goes keeps its place while it folds away.
        let cards = RailCard::ALL
            .into_iter()
            .filter_map(|card| {
                let shown = self.motion.shown(
                    format!("rail-card-{}", card.key()),
                    cards.contains(&card).then_some(()),
                    SHOW,
                    window,
                    cx,
                )?;
                Some(self.render_rail_card(card, shown.progress(), shown.leaving(), window, cx))
            })
            .collect();
        let stack = stack::RailStack {
            id: "chat-rail-cards".into(),
            cards,
            gap: cx.theme().tokens.spacing.sm,
        };
        Some(
            slot(body.width, presence)
                .debug_selector(|| "chat-rail".into())
                .child(crate::motion::faded::faded(presence, stack))
                .when(!body.rail, |rail| rail.child(crate::motion::shield()))
                .into_any_element(),
        )
    }

    /// One card, `presence` of the way in: its header names it and sums
    /// it up, and its body shows below in whatever height the rail gives
    /// it, scrolling inside it or fitting its rows to it.
    fn render_rail_card(
        &self,
        card: RailCard,
        presence: f32,
        leaving: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> stack::StackCard {
        let key = card.key();
        let summary = card.summary(self, cx);
        let header = crate::material::card_header_row(cx)
            .debug_selector(move || format!("rail-card-{key}"))
            .relative()
            .opacity(presence)
            .child(
                Icon::new(card.icon())
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_none()
                    .text_body(false, cx)
                    .font_medium()
                    .text_color(cx.theme().foreground)
                    .child(crate::widgets::fade_text(card.title())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_secondary(false, cx)
                    // A new summary cross-fades over the one it replaces. No
                    // summary is still a line tall: this box clips what
                    // leaves to the one that stays, and a card leaving has
                    // none.
                    .child(self.motion.cross_fade_part(
                        format!("rail-card-summary-{key}"),
                        summary.unwrap_or_default(),
                        SHOW,
                        window,
                        cx,
                        |summary, _, _| {
                            crate::widgets::fade_text(summary.clone()).into_any_element()
                        },
                    )),
            )
            .children(
                card.action(self, cx)
                    .map(|action| div().flex_none().child(action)),
            )
            .into_any_element();
        let fit = card.fit(self, cx);
        let body = card.body(self, window, cx).map(|body| {
            div()
                .id(SharedString::from(format!("rail-card-body-{key}")))
                .relative()
                .size_full()
                .opacity(presence)
                // The body takes the wheel over it rather than the timeline
                // under the rail.
                .occlude()
                .map(|region| {
                    if card.lists_itself() || fit.is_some() {
                        region.child(body)
                    } else {
                        region.child(
                            div().w_full().overflow_y_scrollbar().child(
                                div()
                                    .w_full()
                                    .px(px(CARD_INSET))
                                    .pb(cx.theme().tokens.spacing.sm)
                                    .child(body),
                            ),
                        )
                    }
                })
                // What is leaving takes no clicks.
                .when(leaving, |body| body.child(crate::motion::shield()))
                .into_any_element()
        });
        stack::StackCard {
            chrome: crate::material::card_raised(cx)
                .p_0()
                .size_full()
                .opacity(presence)
                .debug_selector(move || format!("rail-card-frame-{key}"))
                .block_mouse_except_scroll()
                .into_any_element(),
            header,
            body,
            rigid: card.rigid(),
            fit,
            presence,
            scale: (card == RailCard::Screen).then(|| screen::scale(self)),
        }
    }
}

#[cfg(test)]
mod tests {
    use agent::{Goal, GoalStatus, PlanStep, PlanStepStatus};
    use gpui::{AppContext as _, Bounds, TestAppContext, VisualTestContext, size};
    use tcode_core::session::Timeline;

    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::draw_settled;
    use crate::window_state::WindowState;

    /// A thread with a long goal and a long plan, whose latest turn ran
    /// `commands` commands.
    fn timeline(commands: usize) -> Timeline {
        let mut timeline = synthetic_markdown_timeline(2);
        timeline.goal = Some(Goal {
            objective: "Keep the rail inside the window however many cards it holds, \
                        scrolling each card's body rather than the rail"
                .into(),
            status: GoalStatus::Active,
            tokens_used: Some(12_000),
            token_budget: Some(200_000),
            time_used_secs: Some(90),
            checks: Some(2),
            last_check: Some("The plan card still pushes the others off the rail".into()),
        });
        timeline.plan_steps = (0..14)
            .map(|step| PlanStep {
                step: format!("Step {step} of a plan long enough to fill its card"),
                status: PlanStepStatus::Pending,
            })
            .collect();
        timeline
            .entries
            .extend((0..commands).map(|index| at_turn(command(&format!("cmd-{index}")), 1)));
        timeline
    }

    /// A chat window short enough that a few cards fill its rail, showing
    /// the first of two threads with `timeline`, settled.
    fn short_rail(
        cx: &mut TestAppContext,
        timeline: impl Fn() -> Timeline,
    ) -> (TwoThreads, &mut VisualTestContext) {
        let mut threads = seed_two_threads(cx, timeline(), Timeline::default());
        let store = threads.store.clone();
        let window_state = cx.new(|_| WindowState::default());
        let (chat, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(store, window_state, Some(threads.first.clone()), window, cx)
        });
        cx.simulate_resize(size(px(1_400.), px(560.)));
        let first = threads.first.clone();
        threads.bind(&chat, &first, cx);
        draw_settled(cx);
        (threads, cx)
    }

    fn bounds(cx: &mut VisualTestContext, selector: &str) -> Option<Bounds<Pixels>> {
        cx.debug_bounds(selector.to_owned().leak())
    }

    /// In a rail too short for every command, the commands card lists the
    /// newest ones and counts the rest.
    #[gpui::test]
    fn a_short_rail_lists_the_newest_commands_and_counts_the_rest(cx: &mut TestAppContext) {
        const COMMANDS: usize = 30;
        let (_threads, cx) = short_rail(cx, || timeline(COMMANDS));
        let newest = (0..COMMANDS)
            .rev()
            .take_while(|index| bounds(cx, &format!("rail-command-cmd-{index}")).is_some())
            .count();
        let listed = (0..COMMANDS)
            .filter(|index| bounds(cx, &format!("rail-command-cmd-{index}")).is_some())
            .count();
        assert!(listed > 0, "the card lists a command");
        assert_eq!(listed, newest, "the card lists the newest commands");
        assert!(listed < COMMANDS, "a short rail holds only some commands");
        assert!(
            bounds(cx, "rail-commands-more").is_some(),
            "the rest are counted"
        );
    }
}
