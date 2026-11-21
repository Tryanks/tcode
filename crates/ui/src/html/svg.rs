//! The pictures of an HTML visual: inline `<svg>` and `data:image/*`, drawn
//! on the background executor and held while the visual shows them.
//!
//! An inline `<svg>` is rewritten before `usvg` draws it, because `usvg`
//! knows nothing of the page around it: `var(--...)` and `currentColor`
//! take their values from the element, the page's `<style>` rules are put
//! inside it, and whatever would make `usvg` read a file is taken out.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::sync::Arc;

use base64::Engine as _;
use gpui::{Context, Hsla, Pixels, RenderImage, SharedString, Size, SvgRenderer, Task, px, size};

use super::HtmlState;
use super::color::hsla_to_css;
use super::css::{Vars, number, resolve_vars_in_text};
use super::raster::{self, Picture, Recent};

/// The longest SVG text handed to the rasterizer. Every `var()` in it is
/// replaced and the page's stylesheets are added to it, so its length is not
/// its author's.
const MAX_SVG_BYTES: usize = 4 * 1024 * 1024;

/// The size an `<svg>` that names none is drawn at.
const UNSIZED: Size<f32> = Size {
    width: 480.0,
    height: 240.0,
};

#[derive(Clone)]
pub(crate) enum RasterState {
    Ready(Picture),
    Failed(SharedString),
}

/// One drawing of one picture: what is drawn, and the scale it is drawn at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RasterKey {
    pub(crate) content: u64,
    scale: u32,
}

impl RasterKey {
    /// An SVG of `natural` size shown `shown` times that large. The scale
    /// goes up in powers of two, so a visual resized a little keeps the
    /// picture it has.
    pub(crate) fn svg(content: u64, natural: Size<f32>, shown: f32) -> (Self, f32) {
        let step = if shown.is_finite() && shown > 0.0 {
            2f32.powf(shown.log2().ceil())
        } else {
            1.0
        };
        let scale = raster::scale(natural, step);
        let key = Self {
            content,
            scale: scale.to_bits(),
        };
        (key, scale)
    }

    /// A picture with pixels of its own, drawn as it is.
    pub(crate) fn image(content: u64) -> Self {
        Self { content, scale: 0 }
    }
}

thread_local! {
    static RECENT: RefCell<Recent<RasterKey, RasterState>> =
        RefCell::new(Recent::new(|state| match state {
            RasterState::Ready(picture) => picture.bytes(),
            RasterState::Failed(_) => 0,
        }));
}

type Drawn = Result<(Arc<RenderImage>, Size<Pixels>), SharedString>;

/// The pictures one visual shows, and those on their way.
#[derive(Default)]
pub(crate) struct MediaSlots {
    resolved: HashMap<RasterKey, RasterState>,
    in_flight: HashMap<RasterKey, Task<()>>,
}

impl MediaSlots {
    pub(crate) fn clear(&mut self) {
        self.resolved.clear();
        self.in_flight.clear();
    }

    pub(crate) fn len(&self) -> usize {
        self.resolved.len() + self.in_flight.len()
    }

    pub(crate) fn holds(&self, key: RasterKey) -> bool {
        self.resolved.contains_key(&key)
    }

    /// The drawing `key` names, held here or drawn recently; failing that,
    /// the same picture at another scale, to show until this one is drawn.
    pub(crate) fn find(&self, key: RasterKey) -> Option<(RasterKey, RasterState)> {
        if let Some(state) = self.resolved.get(&key) {
            return Some((key, state.clone()));
        }
        if let Some(state) = RECENT.with(|recent| recent.borrow_mut().get(&key)) {
            return Some((key, state));
        }
        self.resolved
            .iter()
            .find(|(held, state)| {
                held.content == key.content && matches!(state, RasterState::Ready(_))
            })
            .map(|(held, state)| (*held, state.clone()))
    }

    /// Let go of every drawing but those the last frame used.
    pub(crate) fn retain(&mut self, used: &HashSet<RasterKey>) {
        self.resolved.retain(|key, _| used.contains(key));
        self.in_flight.retain(|key, _| used.contains(key));
    }

    /// Hold the drawing `key` names: the recent one, or `draw`'s, made on the
    /// background executor.
    pub(crate) fn ensure(
        &mut self,
        key: RasterKey,
        draw: impl FnOnce(&SvgRenderer) -> Drawn + Send + 'static,
        cx: &mut Context<HtmlState>,
    ) {
        if self.resolved.contains_key(&key) || self.in_flight.contains_key(&key) {
            return;
        }
        if let Some(recent) = RECENT.with(|recent| recent.borrow_mut().get(&key)) {
            self.resolved.insert(key, recent);
            return;
        }
        let renderer = cx.svg_renderer();
        let drawing = cx
            .background_executor()
            .spawn(async move { draw(&renderer) });
        let task = cx.spawn(async move |this, cx| {
            let drawn = drawing.await;
            let _ = this.update(cx, |state, cx| {
                let outcome = match drawn {
                    Ok((image, size)) => RasterState::Ready(Picture::new(image, size, cx)),
                    Err(error) => RasterState::Failed(error),
                };
                RECENT.with(|recent| recent.borrow_mut().insert(key, outcome.clone()));
                state.media.in_flight.remove(&key);
                state.media.resolved.insert(key, outcome);
                cx.notify();
            });
        });
        self.in_flight.insert(key, task);
    }
}

/// What is shown for a picture its decoder or rasterizer would not draw,
/// with the reason it gave.
fn failed(error: impl std::fmt::Display) -> SharedString {
    crate::tr!("html.picture_failed", error = error)
        .into_owned()
        .into()
}

pub(crate) fn draw_svg(xml: &str, scale: f32, renderer: &SvgRenderer) -> Drawn {
    raster::rasterize(xml.as_bytes(), scale, renderer).map_err(failed)
}

pub(crate) struct PreparedSvg {
    pub(crate) xml: String,
    pub(crate) natural: Size<f32>,
}

impl PreparedSvg {
    pub(crate) fn content(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        "svg".hash(&mut hasher);
        self.xml.hash(&mut hasher);
        hasher.finish()
    }
}

/// The page's stylesheets as an inline `<svg>` of an element with `vars` and
/// `current_color` takes them, after a rule giving its text the page's font.
/// A sheet that would not fit, or would end the section it is put in, is
/// left out.
pub(crate) fn page_css(
    stylesheets: &[String],
    current_color: Hsla,
    vars: &Vars,
    font_family: &str,
) -> String {
    let color = hsla_to_css(current_color);
    let families = (!font_family.is_empty())
        .then_some(font_family)
        .into_iter()
        .chain(crate::i18n::cjk_fallback_families().iter().copied())
        .map(|family| format!("\"{family}\", "))
        .collect::<String>();
    let mut css = format!("text, tspan {{ font-family: {families}sans-serif; fill: {color}; }}");
    for sheet in stylesheets {
        let Some(resolved) = resolve_vars_in_text(sheet, vars, MAX_SVG_BYTES - css.len()) else {
            continue;
        };
        let resolved = replace_ascii_case_insensitive(&resolved, "currentcolor", &color);
        if !resolved.contains("]]>") && css.len() + resolved.len() < MAX_SVG_BYTES {
            css.push('\n');
            css.push_str(&resolved);
        }
    }
    css
}

/// Preprocess an inline `<svg>` for `usvg`/`resvg`:
/// - resolves `var(--...)` custom properties
/// - replaces `currentColor` with the element's resolved foreground color
/// - takes out references to anything outside the text itself
/// - ensures `xmlns="http://www.w3.org/2000/svg"` is present
/// - puts `css`, the [`page_css`], inside it so classes inside SVG work
/// - determines natural dimensions from `width`/`height`/`viewBox`
///
/// `None` when the result would be longer than [`MAX_SVG_BYTES`].
pub(crate) fn prepare_svg(
    raw_svg: &str,
    current_color: Hsla,
    vars: &Vars,
    css: &str,
) -> Option<PreparedSvg> {
    let xml = resolve_vars_in_text(raw_svg, vars, MAX_SVG_BYTES)?;
    let cur_css = hsla_to_css(current_color);
    let xml = replace_ascii_case_insensitive(&xml, "currentcolor", &cur_css);
    let mut xml = without_external_references(&xml);

    let natural = extract_svg_dimensions(&xml);

    // Ensure `<svg ...>` carries xmlns, default color, and explicit pixel dimensions
    // (usvg cannot rasterize percentage dimensions like `width="100%"` reliably without a viewport).
    if let Some(gt) = open_tag_end(&xml) {
        let open_tag = &xml[..gt];
        let mut additions = String::new();
        if !open_tag.to_ascii_lowercase().contains("xmlns=") {
            additions.push_str(r#" xmlns="http://www.w3.org/2000/svg""#);
        }
        if !open_tag.to_ascii_lowercase().contains("color=") {
            additions.push_str(&format!(r#" color="{cur_css}""#));
        }
        let width_is_pct =
            read_svg_attr(open_tag, "width").is_some_and(|w| w.trim().ends_with('%'));
        let height_is_pct =
            read_svg_attr(open_tag, "height").is_some_and(|h| h.trim().ends_with('%'));
        if read_svg_attr(open_tag, "width").is_none() {
            additions.push_str(&format!(r#" width="{}""#, natural.width.round() as u32));
        }
        if read_svg_attr(open_tag, "height").is_none() {
            additions.push_str(&format!(r#" height="{}""#, natural.height.round() as u32));
        }
        if !additions.is_empty() {
            let insert_at = if xml.as_bytes().get(gt.saturating_sub(1)) == Some(&b'/') {
                gt - 1
            } else {
                gt
            };
            xml.insert_str(insert_at, &additions);
        }
        if width_is_pct {
            xml = replace_pct_svg_attr(&xml, "width", natural.width.round() as u32);
        }
        if height_is_pct {
            xml = replace_pct_svg_attr(&xml, "height", natural.height.round() as u32);
        }
    }

    if let Some(gt) = open_tag_end(&xml)
        && xml.as_bytes().get(gt.saturating_sub(1)) != Some(&b'/')
    {
        xml.insert_str(gt + 1, &format!("<style><![CDATA[{css}]]></style>"));
    }

    (xml.len() <= MAX_SVG_BYTES).then_some(PreparedSvg { xml, natural })
}

pub(crate) fn data_image_content(src: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    "data-img".hash(&mut hasher);
    src.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn draw_data_image(data_uri: &str, renderer: &SvgRenderer) -> Drawn {
    let (meta, payload) = data_uri
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(','))
        .ok_or_else(|| crate::tr!("html.data_uri_malformed").into_owned())?;
    let meta_lower = meta.to_ascii_lowercase();
    let bytes = if meta_lower.contains(";base64") {
        base64::engine::general_purpose::STANDARD
            .decode(payload.trim().as_bytes())
            .map_err(failed)?
    } else {
        percent_encoding::percent_decode_str(payload).collect()
    };

    if meta_lower.starts_with("image/svg+xml") {
        let svg = String::from_utf8(bytes).map_err(failed)?;
        let svg = without_external_references(&svg);
        let natural = extract_svg_dimensions(&svg);
        return draw_svg(&svg, raster::scale(natural, 1.0), renderer);
    }

    let reader = || {
        image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(failed)
    };
    let (w, h) = reader()?.into_dimensions().map_err(failed)?;
    // A few kilobytes of compressed picture can name gigabytes of pixels.
    if w as f32 * h as f32 > raster::MAX_PIXELS {
        return Err(crate::tr!("html.image_too_large", width = w, height = h)
            .into_owned()
            .into());
    }
    let mut bgra = reader()?.decode().map_err(failed)?.into_rgba8();
    // Convert RGBA -> BGRA as GPUI's `RenderImage` expects.
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let frame = image::Frame::new(bgra);
    let render_image = Arc::new(RenderImage::new(vec![frame]));
    Ok((render_image, size(px(w as f32), px(h as f32))))
}

/// `svg` without what makes `usvg` read anything but the text it is given.
///
/// `usvg` reads the file an `<image>` names, relative or absolute, and a
/// document type can declare entities that spell such a name where it cannot
/// be seen. So the document type goes, and with it every entity but XML's
/// own, and so does every `href` (`xlink:href`, or under any other prefix)
/// that is neither a `data:` address nor a `#fragment` of this text. The
/// search is by text, not by element: it also takes an `href="…"` written
/// out in a text node or a comment, which draws nothing.
fn without_external_references(svg: &str) -> String {
    let svg = without_doctype(svg);
    let bytes = svg.as_bytes();
    let is_space = |b: u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');
    let is_name =
        |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.') || b >= 0x80;
    let mut out = String::with_capacity(svg.len());
    let mut copied = 0;
    let mut at = 0;
    while let Some(found) = find_ascii_case_insensitive(&bytes[at..], b"href") {
        let name_end = at + found + 4;
        at = name_end;
        let mut i = name_end;
        while bytes.get(i).copied().is_some_and(is_space) {
            i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while bytes.get(i).copied().is_some_and(is_space) {
            i += 1;
        }
        let Some(&quote) = bytes.get(i).filter(|b| matches!(b, b'"' | b'\'')) else {
            continue;
        };
        let value_start = i + 1;
        let Some(len) = bytes[value_start..].iter().position(|&b| b == quote) else {
            break;
        };
        let value = svg[value_start..value_start + len].trim_start();
        at = value_start + len + 1;
        let internal = value.starts_with('#')
            || value
                .get(..5)
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"));
        if internal {
            continue;
        }
        let mut name_start = name_end - 4;
        while name_start > copied && is_name(bytes[name_start - 1]) {
            name_start -= 1;
        }
        out.push_str(&svg[copied..name_start]);
        copied = at;
    }
    out.push_str(&svg[copied..]);
    out
}

fn without_doctype(svg: &str) -> Cow<'_, str> {
    let mut svg = Cow::Borrowed(svg);
    while let Some(start) = find_ascii_case_insensitive(svg.as_bytes(), b"<!doctype") {
        let bytes = svg.as_bytes();
        let mut end = start + 9;
        let mut quote = 0u8;
        let mut subsets = 0usize;
        while end < bytes.len() {
            let b = bytes[end];
            end += 1;
            if quote != 0 {
                if b == quote {
                    quote = 0;
                }
                continue;
            }
            match b {
                b'"' | b'\'' => quote = b,
                b'[' => subsets += 1,
                b']' => subsets = subsets.saturating_sub(1),
                b'>' if subsets == 0 => break,
                _ => {}
            }
        }
        svg = Cow::Owned(format!("{}{}", &svg[..start], &svg[end..]));
    }
    svg
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// Where the first tag of `svg` ends: its `>`, past any inside a quoted
/// attribute value.
fn open_tag_end(svg: &str) -> Option<usize> {
    let mut quote = 0u8;
    for (i, b) in svg.bytes().enumerate() {
        if quote != 0 {
            if b == quote {
                quote = 0;
            }
        } else if b == b'"' || b == b'\'' {
            quote = b;
        } else if b == b'>' {
            return Some(i);
        }
    }
    None
}

/// Extract intrinsic `(width, height)` from `<svg>`'s `width`/`height`/`viewBox`.
pub(crate) fn extract_svg_dimensions(svg: &str) -> Size<f32> {
    let open = open_tag_end(svg).map_or(svg, |gt| &svg[..gt]);
    let attr_w = read_svg_attr(open, "width").and_then(parse_svg_number);
    let attr_h = read_svg_attr(open, "height").and_then(parse_svg_number);
    let vb = read_svg_attr(open, "viewBox").and_then(parse_viewbox);

    let natural = match (attr_w, attr_h, vb) {
        (Some(w), Some(h), _) => size(w, h),
        (Some(w), None, Some((vw, vh))) => size(w, w * (vh / vw)),
        (None, Some(h), Some((vw, vh))) => size(h * (vw / vh), h),
        (_, _, Some((vw, vh))) => size(vw, vh),
        (Some(w), _, _) => size(w, w * 0.6),
        (_, Some(h), _) => size(h * 1.5, h),
        _ => UNSIZED,
    };
    // A quotient of two numbers in range may be out of it.
    let drawable = |side: f32| side.is_finite() && side >= 1.0;
    if drawable(natural.width) && drawable(natural.height) {
        natural
    } else {
        UNSIZED
    }
}

fn replace_pct_svg_attr(xml: &str, name: &str, px_val: u32) -> String {
    let Some(gt) = open_tag_end(xml) else {
        return xml.to_string();
    };
    let open = &xml[..gt];
    let Some(val) = read_svg_attr(open, name) else {
        return xml.to_string();
    };
    let needle_dq = format!(r#"{name}="{val}""#);
    let needle_sq = format!(r#"{name}='{val}'"#);
    let replacement = format!(r#"{name}="{px_val}""#);
    if open.contains(&needle_dq) {
        format!(
            "{}{}",
            open.replacen(&needle_dq, &replacement, 1),
            &xml[gt..]
        )
    } else if open.contains(&needle_sq) {
        format!(
            "{}{}",
            open.replacen(&needle_sq, &replacement, 1),
            &xml[gt..]
        )
    } else {
        xml.to_string()
    }
}

fn read_svg_attr<'a>(open_tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = open_tag.as_bytes();
    let needle = name.as_bytes();
    let mut i = 0;
    while i + needle.len() + 1 < bytes.len() {
        let prev_ok = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if prev_ok && bytes[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            let mut j = i + needle.len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'=') {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if let Some(&q) = bytes.get(j)
                    && (q == b'"' || q == b'\'')
                {
                    let start = j + 1;
                    if let Some(end) = open_tag[start..].find(q as char) {
                        return Some(&open_tag[start..start + end]);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn parse_svg_number(s: &str) -> Option<f32> {
    let t = s.trim();
    if t.ends_with('%') {
        return None;
    }
    number(t.strip_suffix("px").unwrap_or(t)).filter(|n| *n > 0.0)
}

fn parse_viewbox(s: &str) -> Option<(f32, f32)> {
    let nums: Vec<f32> = s
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .filter_map(number)
        .collect();
    if nums.len() == 4 && nums[2] > 0.0 && nums[3] > 0.0 {
        Some((nums[2], nums[3]))
    } else {
        None
    }
}

fn replace_ascii_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || h.len() < n.len() {
        return haystack.to_string();
    }
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < h.len() {
        if i + n.len() <= h.len() && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            out.push_str(replacement);
            i += n.len();
        } else {
            let ch = haystack[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::RasterState;
    use crate::html::tests::show;
    use gpui::{ImageSource, TestAppContext, px};

    /// The pixels of the one picture `html` draws, which no test window
    /// paints: read from the picture as `img` reads them to draw it.
    fn pixels(html: &str, cx: &mut TestAppContext) -> Vec<u8> {
        let (host, cx) = show(html, cx);
        let state = host.read_with(cx, |host, _| host.state.clone());
        let source = state.read_with(cx, |state, _| {
            let mut pictures = state.media.resolved.values();
            match (pictures.next(), pictures.next()) {
                (Some(RasterState::Ready(picture)), None) => picture.source(),
                _ => panic!("the visual draws one picture"),
            }
        });
        let ImageSource::Custom(lease) = source else {
            panic!("a picture is leased to what draws it");
        };
        let leased = Rc::new(RefCell::new(None));
        let painted = leased.clone();
        cx.draw(
            gpui::Point::default(),
            gpui::size(px(8.0), px(8.0)),
            |_, _| {
                gpui::canvas(
                    |_, _, _| {},
                    move |_, _, window, cx| *painted.borrow_mut() = lease(window, cx),
                )
            },
        );
        let image = leased.take().unwrap().unwrap();
        image.as_bytes(0).unwrap().to_vec()
    }

    #[gpui::test]
    fn an_svg_draws_no_file_it_names(cx: &mut TestAppContext) {
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let path = std::env::temp_dir().join(format!("tcode-html-{}.png", std::process::id()));
        std::fs::write(&path, png.get_ref()).unwrap();
        let svg = |attribute: &str, href: &str| {
            format!(
                r#"<svg width="8" height="8" xmlns:x="http://www.w3.org/1999/xlink">
                   <image {attribute}="{href}" width="8" height="8"/></svg>"#
            )
        };
        let file = path.to_str().unwrap();
        let named = [
            svg("href", file),
            svg("xlink:href", file),
            svg("x:href", file),
            svg("href", &format!("file://{file}")),
            format!(
                r#"<img src="data:image/svg+xml,{}">"#,
                svg("href", file).replace('"', "'").replace('#', "%23")
            ),
            format!(
                r#"<img src="data:image/svg+xml,<!DOCTYPE svg [<!ENTITY f '{file}'>]>{}">"#,
                svg("href", "&f;").replace('"', "'").replace('#', "%23")
            ),
        ];
        let blank = |pixels: &[u8]| pixels.iter().all(|byte| *byte == 0);
        for html in named {
            let mut cx = cx.new_app();
            // A document type's entity fails the picture; anything else
            // draws it without the file.
            let (host, vcx) = show(&html, &mut cx);
            let state = host.read_with(vcx, |host, _| host.state.clone());
            let failed = state.read_with(vcx, |state, _| {
                let mut pictures = state.media.resolved.values();
                matches!(
                    (pictures.next(), pictures.next()),
                    (Some(RasterState::Failed(_)), None)
                )
            });
            assert!(
                failed || blank(&pixels(&html, &mut cx.new_app())),
                "{html} drew the file"
            );
        }
        // The same picture written into the visual is drawn.
        use base64::Engine as _;
        let inline = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png.get_ref())
        );
        assert!(!blank(&pixels(&svg("href", &inline), cx)));
        std::fs::remove_file(&path).unwrap();
    }
}
