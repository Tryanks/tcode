//! Reusable, pure-Rust static HTML + CSS + inline SVG renderer for GPUI.
//!
//! Translates self-contained HTML fragments and `<style>` blocks directly into
//! native GPUI elements (Flexbox, Grid, tables, gradients, shadows, typography,
//! CSS custom properties, `@media (prefers-color-scheme)`), while rasterizing
//! inline `<svg>` and `data:image/*` asynchronously on the background executor.
//! No JavaScript is executed and no external network URLs are fetched.

mod color;
mod css;
mod dom;
mod render;
mod style;
mod svg;

use std::sync::Arc;

use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, RenderOnce, Styled as _, Window, div,
};

use css::Stylesheet;
use dom::Document;
use render::PendingMedia;
use svg::MediaSlots;

/// Cached parsed state and async media slots for one HTML visual.
///
/// Updating `source` with the same text is a no-op, so scrolling and theme
/// repaints never re-parse the DOM or CSS.
pub struct HtmlState {
    source: Arc<str>,
    pub(crate) doc: Document,
    pub(crate) sheet: Stylesheet,
    pub(crate) media: MediaSlots,
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
        cx.notify();
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    fn dispatch_pending(&mut self, pending: Vec<PendingMedia>, cx: &mut Context<Self>) {
        for item in pending {
            match item {
                PendingMedia::Svg {
                    key,
                    xml,
                    natural,
                    target_width,
                } => {
                    self.media.ensure_svg(key, xml, natural, target_width, cx);
                }
                PendingMedia::DataImage { key, uri } => {
                    self.media.ensure_data_image(key, uri, cx);
                }
            }
        }
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
        let width = self.viewport_width.unwrap_or(fallback_width);
        let (element, pending) = render::render_document(&self.state, width, cx);
        if !pending.is_empty() {
            let state = self.state.clone();
            window.defer(cx, move |_, cx| {
                state.update(cx, |s, cx| s.dispatch_pending(pending, cx));
            });
        }
        div().w_full().min_w_0().child(element)
    }
}

#[cfg(test)]
mod tests {
    use super::color::{CssColor, parse_color};
    use super::css::{MediaEnv, Stylesheet, resolve_vars};
    use super::dom::{Document, NodeKind};
    use super::style::{ComputedStyle, DisplayKind, GridColumns, compute_element_style};
    use super::svg::prepare_svg;
    use crate::theme::ActiveTheme as _;
    use gpui::TestAppContext;

    #[test]
    fn dom_extracts_styles_preserves_svg_and_strips_scripts() {
        let html = r#"
            <style>.card { color: red; }</style>
            <script>window.pwn = 1;</script>
            <div class="card" id="hero">
                <p>Hello &amp; <strong>World</strong> &#x2192; &mdash;
                <p>Second paragraph</p>
                <svg viewBox="0 0 120 40"><rect width="120" height="40" fill="var(--blue)"/></svg>
            </div>
        "#;
        let doc = Document::parse(html);
        assert_eq!(doc.stylesheets.len(), 1);
        assert!(doc.stylesheets[0].contains(".card"));

        // Script content must never enter the DOM.
        for node in &doc.nodes {
            if let NodeKind::Text(t) = &node.kind {
                assert!(!t.contains("pwn"));
            }
        }
        // Implicit `<p>` closing creates two sibling paragraphs and one SVG inside `#hero`.
        let hero_id = doc.roots[0];
        let hero = doc.element(hero_id).unwrap();
        assert_eq!(hero.id.as_deref(), Some("hero"));
        let child_tags: Vec<&str> = doc.nodes[hero_id]
            .children
            .iter()
            .filter_map(|&id| doc.element(id).map(|e| e.tag.as_str()))
            .collect();
        assert_eq!(child_tags, vec!["p", "p", "svg"]);

        let svg_node = doc.nodes[hero_id]
            .children
            .iter()
            .copied()
            .find(|&id| doc.element(id).is_some_and(|e| e.tag == "svg"))
            .unwrap();
        let raw_svg = doc.element(svg_node).unwrap().raw_svg.as_deref().unwrap();
        assert!(raw_svg.contains("viewBox=\"0 0 120 40\""));
        assert!(raw_svg.contains("fill=\"var(--blue)\""));
    }

    #[gpui::test]
    fn css_specificity_important_media_and_vars_resolve_correctly(cx: &mut TestAppContext) {
        let html = r#"
            <style>
                :root { --accent-custom: #112233; }
                @media (prefers-color-scheme: dark) {
                    :root { --accent-custom: #aabbcc; }
                }
                .grid {
                    display: grid;
                    grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
                    color: #000000 !important;
                }
                #main {
                    color: var(--accent-custom);
                }
                ul > li:nth-child(even) {
                    opacity: 0.5;
                }
            </style>
            <div id="main" class="grid" style="opacity: 0.8">
                <ul><li>One</li><li>Two</li></ul>
            </div>
        "#;
        let doc = Document::parse(html);
        let sheet = Stylesheet::parse_all(&doc.stylesheets);

        cx.update(|cx| {
            let theme = cx.theme();
            let dark_env = MediaEnv {
                dark: true,
                viewport_width: 640.0,
            };
            let root = ComputedStyle::root(theme, &sheet, dark_env);
            assert_eq!(
                root.vars.get("--accent-custom").map(String::as_str),
                Some("#aabbcc")
            );

            let main_id = doc.roots[0];
            let main_el = doc.element(main_id).unwrap();
            let main_style =
                compute_element_style(&doc, main_id, main_el, &root, &sheet, theme, dark_env);
            assert_eq!(main_style.display, DisplayKind::Grid);
            assert_eq!(
                main_style.grid_columns,
                Some(GridColumns::AutoFit { min_px: 180.0 })
            );
            // `!important` on `.grid` beats `#main` specificity.
            assert_eq!(
                main_style.color,
                match parse_color("#000000").unwrap() {
                    CssColor::Exact(c) => c,
                    CssColor::CurrentColor => unreachable!(),
                }
            );
            assert_eq!(main_style.opacity, Some(0.8));

            // `:nth-child(even)` matches second `<li>` only.
            let ul_id = doc.nodes[main_id]
                .children
                .iter()
                .copied()
                .find(|&id| doc.element(id).is_some_and(|e| e.tag == "ul"))
                .unwrap();
            let lis: Vec<_> = doc.nodes[ul_id]
                .children
                .iter()
                .copied()
                .filter(|&id| doc.element(id).is_some_and(|e| e.tag == "li"))
                .collect();
            let ul_style = compute_element_style(
                &doc,
                ul_id,
                doc.element(ul_id).unwrap(),
                &main_style,
                &sheet,
                theme,
                dark_env,
            );
            let li1 = compute_element_style(
                &doc,
                lis[0],
                doc.element(lis[0]).unwrap(),
                &ul_style,
                &sheet,
                theme,
                dark_env,
            );
            let li2 = compute_element_style(
                &doc,
                lis[1],
                doc.element(lis[1]).unwrap(),
                &ul_style,
                &sheet,
                theme,
                dark_env,
            );
            assert_eq!(li1.opacity, None);
            assert_eq!(li2.opacity, Some(0.5));
        });
    }

    #[test]
    fn svg_preprocessing_resolves_css_vars_and_dimensions() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("--blue".into(), "#2563eb".into());
        assert_eq!(
            resolve_vars("1px solid var(--missing, var(--blue))", &vars),
            "1px solid #2563eb"
        );
        let prepared = prepare_svg(
            r#"<svg width="100%" viewBox="0 0 200 100"><rect fill="var(--blue)" stroke="currentColor"/></svg>"#,
            gpui::Hsla::black(),
            &vars,
            &[],
            "sans-serif",
        );
        assert!(prepared.xml.contains(r##"fill="#2563eb""##));
        assert!(prepared.xml.contains(r##"stroke="#000000""##));
        assert!(
            prepared
                .xml
                .contains(r#"xmlns="http://www.w3.org/2000/svg""#)
        );
        assert!(prepared.xml.contains(r#"width="200""#));
        assert!(prepared.xml.contains("Noto Sans CJK SC"));
        assert_eq!(prepared.natural, gpui::size(200.0, 100.0));
    }

    #[gpui::test]
    fn split_accent_border_detects_distinct_side_color(cx: &mut TestAppContext) {
        let html = r#"
            <style>
                .callout {
                    border: 1px solid #d1d5db;
                    border-left: 4px solid #2563eb;
                    border-radius: 8px;
                }
            </style>
            <div class="callout">Note</div>
        "#;
        let doc = Document::parse(html);
        let sheet = Stylesheet::parse_all(&doc.stylesheets);
        cx.update(|cx| {
            let theme = cx.theme();
            let env = MediaEnv {
                dark: false,
                viewport_width: 600.0,
            };
            let root = ComputedStyle::root(theme, &sheet, env);
            let id = doc.roots[0];
            let style = compute_element_style(
                &doc,
                id,
                doc.element(id).unwrap(),
                &root,
                &sheet,
                theme,
                env,
            );
            let accent = style
                .split_accent_border()
                .expect("left border has a distinct color from other sides");
            assert_eq!(accent.side, super::style::AccentBorderSide::Left);
            assert_eq!(
                accent.color,
                match parse_color("#2563eb").unwrap() {
                    CssColor::Exact(c) => c,
                    CssColor::CurrentColor => unreachable!(),
                }
            );
        });
    }

    #[test]
    fn color_parser_handles_hex_rgb_hsl_light_dark_and_color_mix() {
        assert!(parse_color("#3b82f6").is_some());
        assert!(parse_color("#fff8").is_some());
        assert!(parse_color("rgb(255 128 0 / 50%)").is_some());
        assert!(parse_color("hsl(210deg 80% 50% / 0.4)").is_some());
        assert!(parse_color("color-mix(in srgb, #ff0000 30%, #0000ff)").is_some());
        assert!(parse_color("color-mix(in oklab, #111111 10%, #ffffff)").is_some());
        assert_eq!(
            super::color::parse_color_for_mode("light-dark(#ffffff, #000000)", false),
            parse_color("#ffffff")
        );
        assert_eq!(
            super::color::parse_color_for_mode("light-dark(#ffffff, #000000)", true),
            parse_color("#000000")
        );
        assert_eq!(parse_color("currentColor"), Some(CssColor::CurrentColor));
    }

    #[gpui::test]
    fn html_view_renders_complex_layout_and_rasterizes_inline_svg(cx: &mut TestAppContext) {
        use super::{HtmlState, HtmlView};
        use gpui::{AppContext as _, Context, Entity, IntoElement, Render, Window};

        struct Host {
            state: Entity<HtmlState>,
        }
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                HtmlView::new(&self.state).viewport_width(640.0)
            }
        }

        let html = r#"
            <style>
                .kpis { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 12px; }
                .card { background: linear-gradient(135deg, var(--blue-bg), var(--card)); border: 1px solid var(--border); border-radius: var(--radius); padding: 12px; }
                .split { display: grid; grid-template-columns: 120px 1fr; gap: 8px; }
                .badge { display: inline-block; padding: 2px 8px; border-radius: 999px; background: var(--green-bg); color: var(--green); }
            </style>
            <div class="kpis">
                <div class="card"><h3>Latency</h3><p><strong>18ms</strong> <span class="badge">OK</span></p></div>
                <div class="card"><h3>Errors</h3><progress value="12" max="100"></progress></div>
            </div>
            <div class="split">
                <div>Sidebar</div>
                <table><thead><tr><th>Service</th><th colspan="2">Status</th></tr></thead>
                <tbody><tr><td>API</td><td>99.9%</td><td><a href="https://example.com">Docs</a></td></tr></tbody></table>
            </div>
            <svg viewBox="0 0 160 60" width="160" height="60">
                <rect x="4" y="10" width="40" height="40" rx="6" fill="var(--chart-1)" />
                <rect x="56" y="20" width="40" height="30" rx="6" fill="var(--chart-2)" />
                <text x="80" y="56" font-size="10" fill="currentColor">Chart</text>
            </svg>
        "#;

        let (view, cx) = cx.add_window_view(|_, cx| Host {
            state: cx.new(|cx| HtmlState::new(html, cx)),
        });
        cx.run_until_parked();
        cx.refresh().unwrap();
        cx.run_until_parked();

        let state = view.read_with(cx, |host, _| host.state.clone());
        let has_ready_svg = state.read_with(cx, |s, _| s.media.has_ready());
        assert!(
            has_ready_svg,
            "inline <svg> must rasterize asynchronously to a Ready image"
        );
    }
}
