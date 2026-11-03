//! The thread's side rail: cards stacked in the room a wide chat leaves
//! right of its content column, each folding open to what it shows.

mod agents;
mod changes;
mod plan;
pub(super) mod screen;

use std::borrow::Cow;
use std::collections::HashSet;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Pixels, Role, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_base::{StyledExt as _, v_flex};
use tcode_core::ui::RightTab;

use super::ChatView;
use crate::icon::{Icon, IconName};
use crate::material::{
    CARD_INSET, CHAT_CONTENT_MAX_WIDTH, CHAT_CONTENT_MIN_PADDING, CHAT_RAIL_EDGE, CHAT_RAIL_GAP,
    CHAT_RAIL_NARROWEST, CHAT_RAIL_WIDEST, PANEL_CONTENT_MIN_WIDTH,
};
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

/// How long the rail, or a card in it, takes to come in or go, and a card's
/// body to fold open or shut.
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
    /// the rail gives way to it.
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
        let side = !compact && side_share.is_some() && fits(body, PANEL_CONTENT_MIN_WIDTH);
        Self {
            body: width,
            rail: !compact && fits(body, CHAT_RAIL_NARROWEST) && has_cards && !side,
            width: px(rail_width),
            reach: px(if compact { 0. } else { reach(body, rail_width) }),
            presence: 0.,
            side,
            side_width: px(side_width),
            side_reach: px(if compact { 0. } else { reach(body, side_width) }),
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
        let rail = self.reach * self.presence;
        let side = self.side_reach * self.side_presence;
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
fn span(width: f32) -> f32 {
    width + CHAT_RAIL_GAP + CHAT_RAIL_EDGE
}

/// Whether something `width` wide fits in the slot of a body `body` wide,
/// beside the widest column moved left as far as the page inset.
fn fits(body: f32, width: f32) -> bool {
    body >= CHAT_CONTENT_MAX_WIDTH + CHAT_CONTENT_MIN_PADDING + span(width)
}

/// How far the column moves left in a body `body` wide for something
/// `width` wide in the slot: nothing while it fits in the gutter right of
/// the centred column, and never so far that the column narrows.
fn reach(body: f32, width: f32) -> f32 {
    (CHAT_CONTENT_MAX_WIDTH + 2. * span(width) - body)
        .clamp(0., span(width) - CHAT_CONTENT_MIN_PADDING)
        .min((body - CHAT_CONTENT_MAX_WIDTH - 2. * CHAT_CONTENT_MIN_PADDING).max(0.))
}

/// The widest an agent's card in the slot of a body `body` wide is: as wide
/// as the widest column leaves.
fn side_widest(body: f32) -> f32 {
    body - CHAT_CONTENT_MAX_WIDTH - CHAT_CONTENT_MIN_PADDING - CHAT_RAIL_GAP - CHAT_RAIL_EDGE
}

/// The slot right of the content column, over the timeline's right gutter,
/// that the rail or an agent's card takes, `width` wide and `presence` of
/// the way in: it fades and slides in from the right, and out to it.
pub(super) fn slot(width: Pixels, presence: f32, cx: &App) -> gpui::Div {
    div()
        .absolute()
        .top(super::timeline_inset(cx))
        .bottom(px(CHAT_CONTENT_MIN_PADDING))
        .right(px(CHAT_RAIL_EDGE - SLIDE * (1. - presence)))
        .w(width)
        .opacity(presence)
}

/// A card the rail can show, in the order it shows them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RailCard {
    Plan,
    Agents,
    Screen,
    Changes,
}

impl RailCard {
    const ALL: [Self; 4] = [Self::Plan, Self::Agents, Self::Screen, Self::Changes];

    /// Its key among the rail's cards and in the view's expansion state.
    fn key(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Agents => "agents",
            Self::Screen => "screen",
            Self::Changes => "changes",
        }
    }

    /// Whether the card shows open until the reader shuts it, rather than
    /// shut until the reader opens it.
    fn open_at_first(self) -> bool {
        matches!(self, Self::Agents)
    }

    /// Its key in the view's expansion state, which holds the cards the
    /// reader turned from how they show at first.
    fn expansion_key(self) -> String {
        if self.open_at_first() {
            format!("rail-shut:{}", self.key())
        } else {
            format!("rail:{}", self.key())
        }
    }

    fn open(self, expanded: &HashSet<String>) -> bool {
        expanded.contains(&self.expansion_key()) != self.open_at_first()
    }

    fn title(self) -> Cow<'static, str> {
        match self {
            Self::Plan => crate::tr!("chat.rail_plan"),
            Self::Agents => crate::tr!("chat.rail_agents"),
            Self::Screen => crate::tr!("chat.rail_screen"),
            Self::Changes => crate::tr!("chat.rail_changes"),
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Plan => IconName::Map,
            Self::Agents => IconName::Bot,
            Self::Screen => IconName::Monitor,
            Self::Changes => IconName::Files,
        }
    }

    /// The right panel's tab that shows what the card does, which the card
    /// gives way to while it is open.
    fn tab(self) -> Option<RightTab> {
        match self {
            Self::Plan => Some(RightTab::Plan),
            Self::Agents => Some(RightTab::Agents),
            Self::Screen => None,
            Self::Changes => Some(RightTab::Diff),
        }
    }

    fn present(self, chat: &ChatView, cx: &App) -> bool {
        match self {
            Self::Plan => plan::present(chat, cx),
            Self::Agents => agents::present(chat, cx),
            Self::Screen => screen::present(chat, cx),
            Self::Changes => changes::present(chat, cx),
        }
    }

    fn summary(self, chat: &ChatView, cx: &App) -> Option<SharedString> {
        match self {
            Self::Plan => plan::summary(chat, cx),
            Self::Agents => agents::summary(chat, cx),
            Self::Screen => screen::summary(chat, cx),
            Self::Changes => changes::summary(chat, cx),
        }
    }

    fn body(
        self,
        chat: &ChatView,
        window: &mut Window,
        cx: &mut Context<ChatView>,
    ) -> Option<AnyElement> {
        match self {
            Self::Plan => plan::body(chat, window, cx),
            Self::Agents => agents::body(chat, window, cx),
            Self::Screen => screen::body(chat, window, cx),
            Self::Changes => changes::body(chat, window, cx),
        }
    }
}

impl ChatView {
    /// The cards the rail shows for the thread now: those the host reports
    /// something for, less any whose tab the right panel shows beside it.
    pub(super) fn rail_cards(&self, cx: &App) -> Vec<RailCard> {
        let store = self.workspace_store.read(cx);
        // The right panel shows the focused thread's tab, whatever another
        // pane's thread has recorded as its own.
        let panel = (store.active_session_id() == self.session_id)
            .then(|| store.panel_state_for(self.session_id.as_deref()))
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

    /// The rail over the timeline's right gutter, sliding and fading in and
    /// out with the layout's presence; `cards` are those it shows now.
    pub(super) fn render_rail(
        &self,
        cards: Vec<RailCard>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let presence = self.body.presence;
        if presence <= 0. || self.window_state.read(cx).compact {
            return None;
        }
        let gap = cx.theme().tokens.spacing.sm;
        let stack = crate::motion::keyed::keyed_owned(
            &self.motion,
            "rail-cards",
            crate::motion::keyed::Fold::Down,
            cards
                .into_iter()
                .map(|card| (SharedString::from(card.key()), card)),
            SHOW,
            window,
            cx,
            // The space under a card folds with it, so the cards below
            // never jump by it.
            |_, card, window, cx| {
                div()
                    .w_full()
                    .pb(gap)
                    .child(self.render_rail_card(*card, window, cx))
                    .into_any_element()
            },
        )
        .render(
            v_flex()
                .id("chat-rail-cards")
                .w_full()
                .max_h_full()
                .overflow_y_scroll(),
        );
        Some(
            slot(self.body.width, presence, cx)
                .debug_selector(|| "chat-rail".into())
                .child(stack)
                .when(!self.body.rail, |rail| rail.child(crate::motion::shield()))
                .into_any_element(),
        )
    }

    /// One card: its header names it, sums it up and folds its body open
    /// or shut; which cards are open is kept with the thread's other
    /// expansions.
    fn render_rail_card(
        &self,
        card: RailCard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = card.key();
        let open_key = card.expansion_key();
        let open = card.open(&self.expanded);
        let muted = cx.theme().muted_foreground;
        let title = card.title();
        let header = crate::material::accessible_clickable(
            crate::material::card_header_row(cx),
            SharedString::from(format!("rail-card-{key}")),
            Role::Button,
            title.clone(),
            cx,
        )
        .debug_selector(move || format!("rail-card-{key}"))
        .aria_expanded(open)
        .relative()
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, _, cx| {
            if !this.expanded.remove(&open_key) {
                this.expanded.insert(open_key.clone());
            }
            cx.notify();
        }))
        .child(crate::material::row_hover(
            crate::material::radius_card(cx),
            cx,
        ))
        .child(Icon::new(card.icon()).small().text_color(muted))
        .child(
            div()
                .flex_none()
                .text_body(false, cx)
                .font_medium()
                .text_color(cx.theme().foreground)
                .child(title),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_secondary(false, cx)
                // A new summary cross-fades over the one it replaces. No
                // summary is still a line tall: this box clips what leaves
                // to the one that stays, and a card leaving has none.
                .child(self.motion.cross_fade_part(
                    format!("rail-card-summary-{key}"),
                    card.summary(self, cx).unwrap_or_default(),
                    SHOW,
                    window,
                    cx,
                    |summary, _, _| crate::widgets::fade_text(summary.clone()).into_any_element(),
                )),
        )
        .child(
            crate::widgets::Chevron::new(SharedString::from(format!("rail-chevron-{key}")), open)
                .owned(&self.motion)
                .text_color(muted),
        );
        let body = self
            .motion
            .shown(
                format!("rail-card-body-{key}"),
                open.then_some(()),
                SHOW,
                window,
                cx,
            )
            .and_then(|fold| {
                let body = card.body(self, window, cx)?;
                Some(
                    fold.fold(
                        div()
                            .w_full()
                            .px(px(CARD_INSET))
                            .pb(cx.theme().tokens.spacing.sm)
                            .child(body),
                    ),
                )
            });
        crate::material::card_raised(cx)
            .p_0()
            .gap_0()
            .w_full()
            .overflow_hidden()
            .block_mouse_except_scroll()
            .child(header)
            .children(body)
            .into_any_element()
    }
}
