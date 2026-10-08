use std::{cell::RefCell, sync::Arc};

use super::{
    inline::{Inline, InlineState},
    math_layout::{self, FormulaSelection},
    render::RecentCache,
    state::MarkdownState,
};
use crate::{
    scroll::ScrollableElement as _,
    sizing::Sizable as _,
    theme::ActiveTheme as _,
    widgets::{Button, button::ButtonVariants as _},
};
use gpui::{
    AnyElement, App, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Styled as _, Window, div,
};
use gpui_base::{h_flex, v_flex};
use latex_rust::{MathBox, MathFont, MathStyle, layout, parse};

type FormulaCache = RecentCache<(String, bool), Option<Arc<MathBox>>>;

thread_local! {
    static FONT: Option<MathFont> = MathFont::stix_two_math().ok();
    static FORMULAS: RefCell<FormulaCache> = RefCell::new(RecentCache::new(128));
}

pub(super) fn is_math(lang: Option<&str>) -> bool {
    lang.is_some_and(|lang| {
        ["latex", "tex", "math"]
            .iter()
            .any(|name| lang.eq_ignore_ascii_case(name))
    })
}

fn formula(source: &str, display: bool) -> Option<Arc<MathBox>> {
    FORMULAS.with(|cache| {
        cache
            .borrow_mut()
            .get_or_insert_with((source.into(), display), || {
                FONT.with(|font| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        layout(
                            &parse(source).ok()?,
                            font.as_ref()?,
                            if display {
                                MathStyle::Display
                            } else {
                                MathStyle::Text
                            },
                        )
                        .ok()
                        .map(Arc::new)
                    }))
                    .ok()
                    .flatten()
                })
            })
    })
}

pub(super) fn block(
    source: &str,
    selection: &FormulaSelection,
    code: AnyElement,
    path: &str,
    view: &Entity<MarkdownState>,
    window: &Window,
    cx: &mut App,
) -> AnyElement {
    let tree = formula(source, true);
    selection.set_active(false);
    let show_source = view.read(cx).math_sources.contains(path);
    let mut toolbar = h_flex().gap_1();
    for (source_mode, key) in [
        (false, "markdown.math_rendered"),
        (true, "markdown.math_source"),
    ] {
        let (view, path) = (view.clone(), path.to_string());
        #[cfg(test)]
        let selector_path = path.clone();
        let button = Button::new(SharedString::from(format!("math-{source_mode}-{path}")))
            .ghost()
            .label(crate::tr!(key).into_owned())
            .selected(source_mode == show_source)
            .on_click(move |_, window, cx| {
                crate::widgets::stop_click_propagation(window, cx);
                gpui_base::TextSelection::clear(window, cx);
                view.update(cx, |state, cx| {
                    state.set_math_source(&path, source_mode, cx)
                });
            });
        #[cfg(test)]
        let button = {
            let path = selector_path;
            div()
                .debug_selector(move || format!("math-mode-{source_mode}-{path}"))
                .child(button)
        };
        toolbar = toolbar.child(button);
    }
    let copy_view = view.clone();
    let copy_path = path.to_string();
    let copy_source = source.strip_suffix('\n').unwrap_or(source).to_string();
    toolbar = toolbar.child(crate::widgets::copy::copy_button(
        &format!("math-{path}"),
        view.read(cx).copied.is(path),
        crate::window_seam::window_is_compact(window, cx),
        move |_, window, cx| {
            crate::widgets::stop_click_propagation(window, cx);
            copy_view.update(cx, |state, cx| {
                state.copy_code(copy_path.clone(), copy_source.clone(), cx)
            });
        },
        cx,
    ));
    let body = if show_source {
        code
    } else if let Some(tree) = tree {
        div()
            .p_3()
            .w_full()
            .child(
                div()
                    .child(math_layout::render(&tree, selection, view, cx))
                    .overflow_x_scroll_area(),
            )
            .into_any_element()
    } else {
        v_flex()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::tr!("markdown.math_error").into_owned()),
            )
            .child(code)
            .into_any_element()
    };
    let block = v_flex()
        .id(format!("math-block-{path}"))
        .w_full()
        .gap_1()
        .child(toolbar)
        .child(body);
    #[cfg(test)]
    let block = {
        let path = path.to_string();
        block.debug_selector(move || format!("markdown-math-{path}"))
    };
    block.into_any_element()
}

pub(super) fn inline(
    math: &super::math_parse::MathSpan,
    raw_state: &Arc<std::sync::Mutex<InlineState>>,
    path: &str,
    view: &Entity<MarkdownState>,
    _: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let show_source = view.read(cx).math_sources.contains(path);
    math.selection.set_active(false);
    let body = if !show_source && let Some(tree) = formula(&math.source, false) {
        math_layout::render(&tree, &math.selection, view, cx)
    } else {
        div()
            .font_family(cx.theme().mono_font_family.clone())
            .child(Inline::new(
                "math-source",
                view.clone(),
                raw_state.clone(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .into_any_element()
    };
    let (view, path) = (view.clone(), path.to_string());
    let label = crate::tr!(if show_source {
        "markdown.math_rendered"
    } else {
        "markdown.math_source"
    })
    .into_owned();
    h_flex()
        .id(format!("math-inline-container-{path}"))
        .child(body)
        .child(
            Button::new(SharedString::from(format!("math-inline-{path}")))
                .ghost()
                .xsmall()
                .label("</>")
                .aria_label(label.clone())
                .tooltip(label)
                .on_click(move |_, window, cx| {
                    crate::widgets::stop_click_propagation(window, cx);
                    gpui_base::TextSelection::clear(window, cx);
                    view.update(cx, |state, cx| {
                        state.set_math_source(&path, !show_source, cx)
                    });
                }),
        )
        .into_any_element()
}
