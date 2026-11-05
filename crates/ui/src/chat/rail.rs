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

use std::borrow::Cow;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    Pixels, SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_base::StyledExt as _;
use tcode_core::ui::RightTab;

use super::ChatView;
use crate::icon::{Icon, IconName};
use crate::material::{
    CARD_INSET, CHAT_CONTENT_MAX_WIDTH, CHAT_CONTENT_MIN_PADDING, CHAT_RAIL_EDGE, CHAT_RAIL_GAP,
    CHAT_RAIL_NARROWEST, CHAT_RAIL_WIDEST, CHAT_SIDE_GRIP, PANEL_CONTENT_MIN_WIDTH,
};
use crate::scroll::ScrollableElement as _;
use crate::sizing::Sizable as _;
use crate::theme::ActiveTheme as _;
use crate::typography::TextRole as _;

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
    /// Whether the body is compact, where nothing takes the slot.
    compact: bool,
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
            && fits(body, PANEL_CONTENT_MIN_WIDTH + CHAT_SIDE_GRIP);
        Self {
            body: width,
            rail: !compact && fits(body, CHAT_RAIL_NARROWEST) && has_cards && !side,
            width: px(rail_width),
            reach: px(if compact { 0. } else { reach(body, rail_width) }),
            presence: 0.,
            side,
            side_width: px(side_width),
            side_reach: px(if compact {
                0.
            } else {
                reach(body, side_width + CHAT_SIDE_GRIP)
            }),
            side_presence: 0.,
            compact,
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

    /// The right inset of the timeline's scrollbar track: left of what takes
    /// the slot, and of the grip of an agent's card `side_width` wide, in
    /// the gap the column keeps clear; at the body's edge with nothing there.
    /// It takes `side_width` rather than the layout's own because a card
    /// leaving keeps the width it had.
    pub(super) fn track_inset(&self, side_width: Pixels) -> Pixels {
        if self.compact {
            return px(0.);
        }
        self.blend(
            self.width + px(CHAT_RAIL_EDGE),
            side_width + px(CHAT_SIDE_GRIP + CHAT_RAIL_EDGE),
        )
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
}

impl RailCard {
    const ALL: [Self; 7] = [
        Self::Goal,
        Self::Plan,
        Self::Agents,
        Self::Commands,
        Self::Screen,
        Self::Changes,
        Self::PullRequests,
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
        }
    }

    /// The right panel's tab that shows what the card does, which the card
    /// gives way to while it is open.
    fn tab(self) -> Option<RightTab> {
        match self {
            Self::Goal | Self::Commands | Self::Screen => None,
            Self::Plan => Some(RightTab::Plan),
            Self::Agents => Some(RightTab::Agents),
            Self::Changes => Some(RightTab::Diff),
            Self::PullRequests => Some(RightTab::PullRequests),
        }
    }

    fn present(self, chat: &ChatView, cx: &App) -> bool {
        match self {
            Self::Goal => goal::present(chat, cx),
            Self::Plan => plan::present(chat, cx),
            Self::Agents => agents::present(chat, cx),
            Self::Commands => commands::present(chat, cx),
            Self::Screen => screen::present(chat, cx),
            Self::Changes => changes::present(chat, cx),
            Self::PullRequests => pull_requests::present(chat, cx),
        }
    }

    /// What its header says beside its title, where that is a fact the
    /// top of its body does not already show.
    fn summary(self, chat: &ChatView, cx: &App) -> Option<SharedString> {
        match self {
            Self::Goal => goal::summary(chat, cx),
            Self::Plan => plan::summary(chat, cx),
            Self::Agents => agents::summary(chat, cx),
            Self::Commands => commands::summary(chat, cx),
            Self::Changes => changes::summary(chat, cx),
            Self::Screen | Self::PullRequests => None,
        }
    }

    fn body(
        self,
        chat: &ChatView,
        window: &mut Window,
        cx: &mut Context<ChatView>,
    ) -> Option<AnyElement> {
        match self {
            Self::Goal => goal::body(chat, window, cx),
            Self::Plan => plan::body(chat, window, cx),
            Self::Agents => agents::body(chat, window, cx),
            Self::Commands => commands::body(chat, window, cx),
            Self::Screen => screen::body(chat, window, cx),
            Self::Changes => changes::body(chat, window, cx),
            Self::PullRequests => pull_requests::body(chat, window, cx),
        }
    }
}

impl ChatView {
    /// The cards the rail shows for the thread now: those the host reports
    /// something for, less any whose tab the right panel shows beside it.
    pub(super) fn rail_cards(&self, cx: &App) -> Vec<RailCard> {
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
    /// out with the layout's presence; `cards` are those it shows now.
    pub(super) fn render_rail(
        &self,
        cards: Vec<RailCard>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let presence = self.body.presence;
        // Laid out from the frame it starts to come in, unseen: an agent
        // closing shrinks toward its row from the frame after.
        if (presence <= 0. && !self.body.rail) || self.window_state.read(cx).compact {
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
            slot(self.body.width, presence)
                .debug_selector(|| "chat-rail".into())
                .child(stack)
                .when(!self.body.rail, |rail| rail.child(crate::motion::shield()))
                .into_any_element(),
        )
    }

    /// One card, `presence` of the way in: its header names it and sums
    /// it up, and its body shows below in whatever height the rail gives
    /// it, scrolling inside it.
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
                    .child(card.title()),
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
            .into_any_element();
        let body = card.body(self, window, cx).map(|body| {
            div()
                .id(SharedString::from(format!("rail-card-body-{key}")))
                .debug_selector(move || format!("rail-card-body-{key}"))
                .relative()
                .size_full()
                .opacity(presence)
                // The body takes the wheel over it rather than the timeline
                // under the rail.
                .occlude()
                .child(
                    div().w_full().overflow_y_scrollbar().child(
                        div()
                            .w_full()
                            .px(px(CARD_INSET))
                            .pb(cx.theme().tokens.spacing.sm)
                            .child(body),
                    ),
                )
                // What is leaving takes no clicks.
                .when(leaving, |body| body.child(crate::motion::shield()))
                .into_any_element()
        });
        stack::StackCard {
            chrome: crate::material::shadow_outside(
                crate::material::card_raised(cx)
                    .p_0()
                    .size_full()
                    .opacity(presence)
                    .debug_selector(move || format!("rail-card-frame-{key}"))
                    .block_mouse_except_scroll(),
                crate::material::radius_card(cx),
            )
            .into_any_element(),
            header,
            body,
            presence,
            scale: (card == RailCard::Screen).then(|| screen::scale(self)),
        }
    }
}

#[cfg(test)]
mod tests {
    use agent::{Goal, GoalStatus, PlanStep, PlanStepStatus};
    use gpui::{AppContext as _, Bounds, TestAppContext, VisualTestContext, point, size};
    use tcode_core::session::Timeline;

    use super::*;
    use crate::chat::tests::*;
    use crate::test_frames::{draw, draw_settled};
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

    const AT_REST: &[&str] = &["frame", "body"];

    fn bounds(cx: &mut VisualTestContext, selector: &str) -> Option<Bounds<Pixels>> {
        cx.debug_bounds(selector.to_owned().leak())
    }

    /// The `parts` of every card lie inside the rail. A card coming in is
    /// drawn clipped to its surface, so only that is its extent then.
    fn inside(cx: &mut VisualTestContext, keys: &[&str], parts: &[&str], when: &str) {
        let rail = bounds(cx, "chat-rail").expect("the rail is drawn");
        let slack = px(0.5);
        for key in keys {
            for part in parts {
                let Some(part) = bounds(cx, &format!("rail-card-{part}-{key}")) else {
                    continue;
                };
                assert!(
                    part.top() >= rail.top() - slack && part.bottom() <= rail.bottom() + slack,
                    "{when}: the {key} card's {part:?} leaves the rail {rail:?}"
                );
            }
        }
    }

    /// Every card in `keys` is drawn on its surface: the card's fill under
    /// its header, as wide as the rail.
    fn on_surfaces(cx: &mut VisualTestContext, keys: &[&str], when: &str) {
        let rail = bounds(cx, "chat-rail").expect("the rail is drawn");
        for key in keys {
            let header = bounds(cx, &format!("rail-card-{key}")).expect("the header shows");
            let surface = cx.update(|window, cx| {
                let scale = window.scale_factor();
                let fill = cx.theme().popover;
                let (top, left) = (header.top().scale(scale), rail.left().scale(scale));
                let width = rail.size.width.scale(scale).0 - 1.;
                let near = |a: gpui::ScaledPixels, b: gpui::ScaledPixels| (a - b).0.abs() <= 1.;
                crate::test_frames::painted_quads(window)
                    .iter()
                    .any(|quad| {
                        quad.background.as_solid() == Some(fill)
                            && near(quad.bounds.top(), top)
                            && near(quad.bounds.left(), left)
                            && quad.bounds.size.width.0 >= width
                    })
            });
            assert!(
                surface,
                "{when}: the {key} card is drawn without its surface"
            );
        }
    }

    /// Cards with more to show than a short rail holds share its height:
    /// the rail never runs past its bottom, as a card comes in or once it
    /// is in, and each card's body scrolls inside its share, on its card's
    /// surface whatever height its share comes to.
    #[gpui::test]
    fn cards_share_a_short_rail_and_scroll_inside_it(cx: &mut TestAppContext) {
        let mut threads = seed_two_threads(cx, timeline(0));
        let store = threads.store.clone();
        let window_state = cx.new(|_| WindowState::new(false));
        let (chat, cx) = crate::chat::tests::add_chat_window(cx, |window, cx| {
            ChatView::new(store, window_state, Some(threads.first.clone()), window, cx)
        });
        cx.simulate_resize(size(px(1_400.), px(560.)));
        let first = threads.first.clone();
        threads.bind(&chat, &first, timeline(0), cx);
        draw_settled(cx);
        assert!(
            cx.update(|_, cx| !cx.reduce_motion()),
            "the cards come in with motion"
        );
        inside(cx, &["goal", "plan"], AT_REST, "at rest with two cards");

        // The commands card comes in over frames, the others giving way.
        threads.store.update(cx, |store, cx| {
            store.set_session_replica_for_test(first.clone(), timeline(9), cx)
        });
        draw(cx);
        for frame in 0..30 {
            inside(
                cx,
                &["goal", "plan", "commands"],
                &["frame"],
                &format!("frame {frame}"),
            );
            on_surfaces(cx, &["goal", "plan"], &format!("frame {frame}"));
            cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(16));
        }
        draw_settled(cx);
        inside(
            cx,
            &["goal", "plan", "commands"],
            AT_REST,
            "at rest with three cards",
        );
        on_surfaces(
            cx,
            &["goal", "plan", "commands"],
            "at rest with three cards",
        );

        let rail = bounds(cx, "chat-rail").unwrap();
        for key in ["goal", "plan", "commands"] {
            let header = bounds(cx, &format!("rail-card-{key}")).expect("the header shows");
            assert!(header.bottom() <= rail.bottom(), "the {key} header shows");
            let body = bounds(cx, &format!("rail-card-body-{key}")).expect("the body shows");
            assert!(body.size.height > px(0.), "the {key} body has room");
        }

        // The commands' body holds more rows than its share and scrolls.
        let body = bounds(cx, "rail-card-body-commands").unwrap();
        let oldest = bounds(cx, "rail-command-cmd-3").expect("the sixth row is listed");
        assert!(
            oldest.bottom() > body.bottom(),
            "the commands body shows all its rows ({oldest:?} in {body:?}): nothing to scroll"
        );
        cx.simulate_mouse_move(body.center(), None, gpui::Modifiers::default());
        cx.simulate_event(gpui::ScrollEvent {
            position: body.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-400.))),
            touch_phase: gpui::TouchPhase::Moved,
            ..Default::default()
        });
        draw_settled(cx);
        let scrolled = bounds(cx, "rail-command-cmd-3").unwrap();
        assert!(
            scrolled.top() < oldest.top(),
            "the commands body scrolls inside its card"
        );
        assert_eq!(
            bounds(cx, "rail-card-body-commands").unwrap(),
            body,
            "the card stays where it is"
        );
        inside(
            cx,
            &["goal", "plan", "commands"],
            AT_REST,
            "after scrolling",
        );
    }
}
