//! Reusable, pure-Rust static HTML + CSS + inline SVG renderer for GPUI.
//!
//! Translates self-contained HTML fragments and `<style>` blocks directly into
//! native GPUI elements (Flexbox, Grid, tables, gradients, shadows, typography,
//! CSS custom properties, `@media (prefers-color-scheme)`), while rasterizing
//! inline `<svg>` and `data:image/*` asynchronously on the background executor.
//! No JavaScript is executed, and nothing is read but the text it is given:
//! no file and no network address.
//!
//! The text is a model's, so every part of this module takes it as hostile:
//! what it costs to parse, to cascade and to draw is bounded here, and the
//! view says so under the visual when a bound left something out.

mod color;
mod css;
mod dom;
pub(crate) mod raster;
mod render;
mod style;
mod svg;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, RenderOnce,
    Size, Styled as _, Window, div,
};

use css::{Budget, MediaEnv, PseudoElement, Stylesheet};
use dom::{Document, NodeId};
use render::{Frame, Wanted};
use style::{Cascade, ComputedStyle, DisplayKind, Palette};
use svg::MediaSlots;

use crate::theme::ActiveTheme as _;

/// Cached parsed state and async media slots for one HTML visual.
///
/// Setting the same text again is a no-op, and the styles are computed once
/// for a theme and the media queries a width answers, so scrolling and
/// repaints neither parse nor cascade.
pub struct HtmlState {
    source: Arc<str>,
    doc: Document,
    sheet: Stylesheet,
    media: MediaSlots,
    computed: Option<Rc<Computed>>,
}

impl HtmlState {
    pub fn new(html: impl Into<Arc<str>>, _cx: &mut Context<Self>) -> Self {
        let source: Arc<str> = html.into();
        let doc = Document::parse(&source);
        let sheet = Stylesheet::parse_all(&doc.stylesheets);
        Self {
            source,
            doc,
            sheet,
            media: MediaSlots::default(),
            computed: None,
        }
    }

    /// Replace the HTML source only when its contents actually changed.
    pub fn set_html(&mut self, html: &str, cx: &mut Context<Self>) {
        if self.source.as_ref() == html {
            return;
        }
        self.source = Arc::from(html);
        self.doc = Document::parse(&self.source);
        self.sheet = Stylesheet::parse_all(&self.doc.stylesheets);
        self.media.clear();
        self.computed = None;
        cx.notify();
    }

    fn computed(&mut self, theme: Palette, env: MediaEnv) -> Rc<Computed> {
        let conditions = self.sheet.conditions(env);
        if let Some(computed) = &self.computed
            && computed.theme == theme
            && computed.conditions == conditions
        {
            return computed.clone();
        }
        let computed = Rc::new(Computed::new(&self.doc, &self.sheet, theme, conditions));
        self.computed = Some(computed.clone());
        computed
    }

    /// Hold the drawings the frame used, start those it had none of, and let
    /// go of the rest: those of a theme or a size no longer shown.
    fn settle(&mut self, frame: Frame, cx: &mut Context<Self>) {
        self.media.retain(&frame.used);
        let Some(computed) = self.computed.clone() else {
            return;
        };
        for wanted in frame.wanted {
            match wanted {
                Wanted::Svg { node, key, scale } => {
                    let Some(prepared) = computed.prepare_svg(&self.doc, node) else {
                        continue;
                    };
                    if prepared.content() == key.content {
                        self.media.ensure(
                            key,
                            move |renderer| svg::draw_svg(&prepared.xml, scale, renderer),
                            cx,
                        );
                    }
                }
                Wanted::DataImage { node, key } => {
                    let src = self.doc.element(node).and_then(|el| el.attr("src"));
                    if let Some(uri) = src.map(str::to_owned) {
                        self.media.ensure(
                            key,
                            move |renderer| svg::draw_data_image(&uri, renderer),
                            cx,
                        );
                    }
                }
            }
        }
    }
}

/// A document's styles under one theme and one answer to its media queries.
struct Computed {
    theme: Palette,
    conditions: Vec<bool>,
    root: ComputedStyle,
    /// By node; `None` for text and for what is inside an element not drawn.
    styles: Vec<Option<ComputedStyle>>,
    before: HashMap<NodeId, ComputedStyle>,
    after: HashMap<NodeId, ComputedStyle>,
    pictures: HashMap<NodeId, PictureOf>,
    /// The cascade's [`Budget`] ran out, or a value was drawn at a limit.
    left_out: bool,
}

/// The picture an `<svg>` or an `<img>` draws.
enum PictureOf {
    Svg { content: u64, natural: Size<f32> },
    SvgTooLong,
    DataImage { content: u64 },
}

impl Computed {
    fn new(doc: &Document, sheet: &Stylesheet, theme: Palette, conditions: Vec<bool>) -> Self {
        let mut cascade = Cascade {
            doc,
            rules: sheet.active(&conditions),
            theme: &theme,
            budget: Budget::new(Budget::CASCADE),
            clamped: false,
        };
        let root = cascade.root();
        let mut computed = Self {
            conditions: Vec::new(),
            styles: Vec::new(),
            before: HashMap::new(),
            after: HashMap::new(),
            pictures: HashMap::new(),
            left_out: false,
            root,
            theme: theme.clone(),
        };
        computed.styles.resize_with(doc.nodes.len(), || None);
        let root = computed.root.clone();
        computed.descend(&doc.roots, &root, &mut cascade, &mut Vec::new());
        computed.left_out = cascade.budget.ran_out() || cascade.clamped;
        drop(cascade);
        computed.conditions = conditions;
        computed
    }

    fn descend(
        &mut self,
        nodes: &[NodeId],
        parent: &ComputedStyle,
        cascade: &mut Cascade<'_>,
        page_css: &mut Vec<PageCss>,
    ) {
        let doc = cascade.doc;
        for &id in nodes {
            let Some(el) = doc.element(id) else {
                continue;
            };
            let style = cascade.element(id, el, parent);
            if style.display != DisplayKind::None {
                match el.tag.as_str() {
                    "svg" => {
                        let raw = el.raw_svg.as_deref().unwrap_or_default();
                        let css = PageCss::of(page_css, doc, &style, &self.theme);
                        let cost = (raw.len() + css.len()) / 64;
                        if cascade.budget.spend(cost) {
                            let picture =
                                match svg::prepare_svg(raw, style.color, &style.vars, &css) {
                                    Some(prepared) => PictureOf::Svg {
                                        content: prepared.content(),
                                        natural: prepared.natural,
                                    },
                                    None => PictureOf::SvgTooLong,
                                };
                            self.pictures.insert(id, picture);
                        }
                    }
                    "img" => {
                        if let Some(src) = el.attr("src").filter(|src| src.starts_with("data:"))
                            && cascade.budget.spend(src.len() / 64)
                        {
                            let content = svg::data_image_content(src);
                            self.pictures.insert(id, PictureOf::DataImage { content });
                        }
                    }
                    _ => {
                        if let Some(before) = cascade.pseudo(id, &style, PseudoElement::Before) {
                            self.before.insert(id, before);
                        }
                        if let Some(after) = cascade.pseudo(id, &style, PseudoElement::After) {
                            self.after.insert(id, after);
                        }
                        self.descend(&doc.nodes[id].children, &style, cascade, page_css);
                    }
                }
            }
            self.styles[id] = Some(style);
        }
    }

    fn style(&self, id: NodeId) -> Option<&ComputedStyle> {
        self.styles.get(id)?.as_ref()
    }

    /// The style of an element that is drawn.
    fn shown(&self, id: NodeId) -> Option<&ComputedStyle> {
        self.style(id)
            .filter(|style| style.display != DisplayKind::None)
    }

    fn before(&self, id: NodeId) -> Option<&ComputedStyle> {
        self.before.get(&id)
    }

    fn after(&self, id: NodeId) -> Option<&ComputedStyle> {
        self.after.get(&id)
    }

    fn picture(&self, id: NodeId) -> Option<&PictureOf> {
        self.pictures.get(&id)
    }

    /// The text the `<svg>` at `node` is drawn from. It is as long as the
    /// `<svg>` and the page's stylesheets together, so it is made again when
    /// a drawing is wanted and not kept for each `<svg>` between.
    fn prepare_svg(&self, doc: &Document, node: NodeId) -> Option<svg::PreparedSvg> {
        let style = self.style(node)?;
        let raw = doc.element(node)?.raw_svg.as_deref()?;
        let css = PageCss::of(&mut Vec::new(), doc, style, &self.theme);
        svg::prepare_svg(raw, style.color, &style.vars, &css)
    }
}

/// The page's stylesheets as the `<svg>`s of one scope of custom properties
/// and one text colour take them: the same text for nearly every `<svg>` of
/// a page, resolved once.
struct PageCss {
    vars: Rc<css::Vars>,
    color: gpui::Hsla,
    css: Rc<str>,
}

impl PageCss {
    fn of(
        known: &mut Vec<Self>,
        doc: &Document,
        style: &ComputedStyle,
        theme: &Palette,
    ) -> Rc<str> {
        if let Some(page) = known
            .iter()
            .find(|page| Rc::ptr_eq(&page.vars, &style.vars) && page.color == style.color)
        {
            return page.css.clone();
        }
        let css: Rc<str> = svg::page_css(
            &doc.stylesheets,
            style.color,
            &style.vars,
            theme.font_sans(),
        )
        .into();
        known.push(Self {
            vars: style.vars.clone(),
            color: style.color,
            css: css.clone(),
        });
        css
    }
}

/// Standalone GPUI component that renders an [`HtmlState`].
#[derive(IntoElement)]
pub struct HtmlView {
    state: Entity<HtmlState>,
    viewport_width: Option<f32>,
}

impl HtmlView {
    pub fn new(state: &Entity<HtmlState>) -> Self {
        Self {
            state: state.clone(),
            viewport_width: None,
        }
    }

    /// Supply the available content width in logical pixels for responsive
    /// `@media (max-width/min-width)`, `auto-fit` grids, and SVG fitting.
    pub fn viewport_width(mut self, width: f32) -> Self {
        self.viewport_width = Some(width);
        self
    }
}

impl RenderOnce for HtmlView {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let fallback_width = (f32::from(window.bounds().size.width) - 64.0).clamp(280.0, 760.0);
        let theme = Palette::of(cx.theme());
        let env = MediaEnv {
            dark: theme.dark,
            viewport_width: self.viewport_width.unwrap_or(fallback_width).max(240.0),
        };
        let computed = self.state.update(cx, |state, _| state.computed(theme, env));
        let state = self.state.read(cx);
        let (element, frame) = render::render_document(state, &computed, env.viewport_width, cx);
        if !frame.wanted.is_empty() || state.media.len() != frame.used.len() {
            let state = self.state.clone();
            window.defer(cx, move |_, cx| {
                state.update(cx, |state, cx| state.settle(frame, cx));
            });
        }
        div()
            .id(("html", self.state.entity_id()))
            .w_full()
            .min_w_0()
            .child(element)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{HtmlState, HtmlView};
    use gpui::{
        AppContext as _, Bounds, Context, Entity, InteractiveElement as _, IntoElement, Modifiers,
        ParentElement as _, Pixels, Render, Styled as _, TestAppContext, VisualTestContext, Window,
        div, point, px,
    };

    /// A visual as the chat's card hosts it.
    pub(super) struct Host {
        pub(super) state: Entity<HtmlState>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            // The root view fills its window; the visual is as tall as it is.
            div().flex().flex_col().items_start().child(
                div()
                    .w(px(640.0))
                    .child(HtmlView::new(&self.state).viewport_width(640.0))
                    .debug_selector(|| "visual".into()),
            )
        }
    }

    /// `html` shown, with every picture it waits for drawn.
    pub(super) fn show<'a>(
        html: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Host>, &'a mut VisualTestContext) {
        cx.update(crate::test_client::init);
        let (host, cx) = cx.add_window_view(|_, cx| Host {
            state: cx.new(|cx| HtmlState::new(html, cx)),
        });
        for _ in 0..2 {
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.refresh();
                _ = window.draw(cx);
            });
        }
        cx.run_until_parked();
        (host, cx)
    }

    fn drawn(html: &str, cx: &mut TestAppContext) -> Bounds<Pixels> {
        let (_, cx) = show(html, cx);
        cx.debug_bounds("visual").expect("the visual is laid out")
    }

    fn nested(open: &str, depth: usize, inner: &str, close: &str) -> String {
        format!("{}{inner}{}", open.repeat(depth), close.repeat(depth))
    }

    /// Text a model could write that crashed, hung or blanked the card, each
    /// with something that must still be drawn beside it.
    #[gpui::test]
    fn hostile_input_is_drawn(cx: &mut TestAppContext) {
        let fan_out: String = (0..16)
            .map(|n| format!("--v{n}:{};", format!("var(--v{})", n + 1).repeat(4)))
            .collect();
        let long_chain = format!("b {} {{ color: red }}", "a ".repeat(30));
        let cases = [
            (
                "a multi-byte value ten bytes from a character boundary",
                r#"<style>.done::after{content:"已完成 ✓"}</style><p class="done">x</p>"#
                    .to_owned(),
            ),
            (
                "custom properties that multiply",
                format!("<style>:root{{{fan_out}--v16:x}}p{{color:var(--v0)}}</style><p>x</p>"),
            ),
            (
                "a custom property that uses itself",
                "<style>:root{--a:var(--a) var(--b);--b:var(--a)}p{color:var(--a)}</style><p>x</p>"
                    .to_owned(),
            ),
            (
                "at-rules nested without end",
                format!(
                    "<style>{}</style><p>x</p>",
                    nested("@media screen{", 20_000, "p{color:red}", "}")
                ),
            ),
            (
                "selector functions nested without end",
                format!(
                    "<style>p{}{{color:red}}</style><p>x</p>",
                    nested(":not(", 20_000, "i", ")")
                ),
            ),
            (
                "colour functions nested without end",
                format!(
                    r#"<p style="color:{}">x</p>"#,
                    nested("light-dark(", 20_000, "red", ",blue)")
                ),
            ),
            (
                "lengths nested without end",
                format!(
                    r#"<p style="width:{};grid-template-columns:{}">x</p>"#,
                    nested("calc(", 20_000, "1px", ")"),
                    nested("minmax(1px,", 20_000, "1fr", ")")
                ),
            ),
            (
                "a descendant selector that fails every way through a deep tree",
                format!(
                    "<style>{long_chain}</style>{}",
                    nested("<a>", 60, "x", "</a>")
                ),
            ),
            (
                "more rules than the cascade tries",
                format!(
                    "<style>{}</style>{}",
                    "div div div span{color:red}".repeat(20_000),
                    "<div><div><div><p>x</p></div></div></div>".repeat(200)
                ),
            ),
            ("a run of ampersands", "&".repeat(300_000)),
            (
                "a head that is never closed",
                "<html><head><title>t</title><p>x</p>".to_owned(),
            ),
            (
                "a pseudo-element this renderer does not draw",
                "<style>::selection{display:none}::-webkit-scrollbar{display:none}</style><p>x</p>"
                    .to_owned(),
            ),
            (
                "media features this renderer does not evaluate",
                "<style>@media (width < 600px){p{display:none}}\
                 @media (orientation: landscape){p{display:none}}</style><p>x</p>"
                    .to_owned(),
            ),
            (
                "inline styles inside one another, around multi-byte text",
                r#"<p><span style="color:red">é <b>é <i style="color:blue">é</i></b> é</span></p>"#
                    .to_owned(),
            ),
            (
                "numbers that are not numbers",
                r#"<div style="width:NaNpx;height:infpx;min-height:1e39px;flex-grow:inf;
                    font-size:NaNpx;line-height:-5px;opacity:NaN;aspect-ratio:1/0;
                    padding:calc(1px / 0);margin:-infpx;font-weight:1e30;
                    box-shadow:NaNpx infpx 1e39px red;border-radius:NaN%">x</div>"#
                    .to_owned(),
            ),
            (
                "grids, spans and lists counted past any layout",
                r#"<style>li:nth-child(-n-2147483647){color:red}</style>
                   <div style="display:grid;grid-template-columns:repeat(65535,1fr);
                               grid-template-rows:repeat(65535,1fr)">
                     <div style="grid-column:1 / 32000;grid-row:span 65535">x</div></div>
                   <table><tr><td colspan="99999999999">x</td></tr></table>
                   <ol start="18446744073709551615"><li>x</li><li>x</li></ol>
                   <progress value="NaN" max="0"></progress><meter value="inf"></meter>
                   <input type="range" min="5" max="5" value="NaN">"#
                    .to_owned(),
            ),
            (
                "a drawing sized past any screen",
                r#"<svg width="40000" height="40000"><rect width="9" height="9"/></svg><p>x</p>"#
                    .to_owned(),
            ),
        ];
        for (case, html) in cases {
            let mut cx = cx.new_app();
            let bounds = drawn(&html, &mut cx);
            assert!(bounds.size.height > px(0.0), "{case}: nothing is drawn");
        }
    }

    #[gpui::test]
    fn what_a_page_would_run_or_embed_is_not_drawn(cx: &mut TestAppContext) {
        let inert = [
            "<script>document.write('<p>written</p>')</script>",
            "<script>never closed <p>text</p>",
            "<iframe src=\"https://example.com\">fallback</iframe>",
            "<template><p>template</p></template>",
            "<object data=\"https://example.com/a.swf\">fallback</object>",
            "<embed src=\"https://example.com/a.swf\">",
            "<link rel=\"stylesheet\" href=\"https://example.com/a.css\">",
            "<img src=\"https://example.com/a.png\">",
            "<img src=\"/etc/hosts\">",
            "<img src=\"file:///etc/hosts\">",
        ];
        for html in inert {
            let mut cx = cx.new_app();
            let bounds = drawn(html, &mut cx);
            assert_eq!(bounds.size.height, px(0.0), "{html} draws something");
        }
        // The same measure sees what is drawn.
        assert!(drawn("<p>text</p>", cx).size.height > px(0.0));
    }

    #[gpui::test]
    fn a_link_opens_only_a_web_or_mail_address(cx: &mut TestAppContext) {
        let refused = [
            "file:///etc/hosts",
            "/etc/hosts",
            "tcode-thread://thread#1",
            "javascript:alert(1)",
            "vscode://file/etc/hosts",
        ];
        for href in refused {
            let mut cx = cx.new_app();
            let (_, cx) = show(&format!(r#"<a href="{href}">link</a>"#), &mut cx);
            let link = cx.debug_bounds("visual").unwrap().origin + point(px(4.0), px(4.0));
            cx.simulate_click(link, Modifiers::default());
            assert_eq!(cx.opened_url(), None, "{href} opened");
        }
        let (_, cx) = show(r#"<a href="https://example.com/a">link</a>"#, cx);
        let link = cx.debug_bounds("visual").unwrap().origin + point(px(4.0), px(4.0));
        cx.simulate_click(link, Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com/a"));
    }

    #[gpui::test]
    fn a_visual_cut_at_a_limit_says_so(cx: &mut TestAppContext) {
        let cases = [
            ("<p>x</p>".repeat(5_000), "html-left-out-nodes"),
            (nested("<div>", 100, "x", "</div>"), "html-left-out-depth"),
            (
                format!(
                    "<style>{}</style>{}",
                    "div div div span{color:red}".repeat(20_000),
                    "<div><div><div><p>x</p></div></div></div>".repeat(200)
                ),
                "html-left-out-styles",
            ),
        ];
        for (html, note) in cases {
            let mut cx = cx.new_app();
            let (_, cx) = show(&html, &mut cx);
            assert!(cx.debug_bounds(note).is_some(), "{note} is not shown");
        }
        let (_, cx) = show("<style>p{color:red}</style><div><p>x</p></div>", cx);
        for note in [
            "html-left-out-nodes",
            "html-left-out-depth",
            "html-left-out-styles",
        ] {
            assert!(cx.debug_bounds(note).is_none(), "{note} is shown");
        }
    }
}
