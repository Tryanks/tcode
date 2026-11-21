//! Asynchronous background rasterization and bounded caching for inline `<svg>`
//! and `data:image/*` elements inside HTML visuals.
//!
//! Inline `<svg>` strings are preprocessed to resolve CSS custom properties
//! (`var(--...)`), `currentColor`, and `<style>` rules before being handed to
//! GPUI's [`SvgRenderer`], because `usvg`/`resvg` does not evaluate CSS
//! variables. Rasterized `RenderImage`s are wrapped in [`crate::store::images::Owned`]
//! so GPU atlas tiles are reclaimed cleanly when visuals leave memory or the
//! theme switches.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::rc::Rc;
use std::sync::Arc;

use base64::Engine as _;
use gpui::{
    App, Context, Hsla, ImageSource, Pixels, RenderImage, SharedString, Size, SvgRenderer, Task,
    px, size,
};

use super::HtmlState;
use super::color::hsla_to_css;
use super::css::resolve_vars;

const MAX_PIXELS: f32 = 8_000_000.0;
const MAX_DISPLAY_HEIGHT: f32 = 900.0;
const CACHE_CAPACITY: usize = 64;
const CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct RasterPicture(Rc<RasterPictureData>);

struct RasterPictureData {
    image: crate::store::images::Owned,
    size: Size<Pixels>,
}

impl RasterPicture {
    fn new(image: Arc<RenderImage>, natural: Size<Pixels>, cx: &mut App) -> Self {
        Self(Rc::new(RasterPictureData {
            image: crate::store::images::Owned::new(image, cx),
            size: natural,
        }))
    }

    pub(crate) fn source(&self) -> ImageSource {
        self.0.image.source()
    }

    pub(crate) fn size(&self) -> Size<Pixels> {
        self.0.size
    }

    fn bytes(&self) -> usize {
        let device = self.0.image.image().size(0);
        device.width.0.max(0) as usize * device.height.0.max(0) as usize * 4
    }
}

#[derive(Clone)]
pub(crate) enum RasterState {
    Ready(RasterPicture),
    Failed(SharedString),
}

#[derive(Default)]
struct GlobalCache {
    entries: VecDeque<(u64, RasterState)>,
    bytes: usize,
}

impl GlobalCache {
    fn get(&mut self, key: u64) -> Option<RasterState> {
        let ix = self.entries.iter().position(|(k, _)| *k == key)?;
        let entry = self.entries.remove(ix)?;
        let state = entry.1.clone();
        self.entries.push_back(entry);
        Some(state)
    }

    fn insert(&mut self, key: u64, state: RasterState) {
        if let Some(ix) = self.entries.iter().position(|(k, _)| *k == key)
            && let Some((_, old)) = self.entries.remove(ix)
        {
            self.bytes = self.bytes.saturating_sub(entry_bytes(&old));
        }
        self.bytes += entry_bytes(&state);
        self.entries.push_back((key, state));
        while self.entries.len() > 1
            && (self.entries.len() > CACHE_CAPACITY || self.bytes > CACHE_MAX_BYTES)
        {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(entry_bytes(&old));
            }
        }
    }
}

fn entry_bytes(state: &RasterState) -> usize {
    match state {
        RasterState::Ready(pic) => pic.bytes(),
        RasterState::Failed(_) => 0,
    }
}

thread_local! {
    static RASTER_CACHE: RefCell<GlobalCache> = RefCell::new(GlobalCache::default());
}

/// Per-`HtmlState` map of pending and completed background rasters keyed by content hash.
#[derive(Default)]
pub(crate) struct MediaSlots {
    resolved: HashMap<u64, RasterState>,
    in_flight: HashMap<u64, Task<()>>,
}

impl MediaSlots {
    pub(crate) fn clear(&mut self) {
        self.resolved.clear();
        self.in_flight.clear();
    }

    pub(crate) fn get(&self, key: u64) -> Option<&RasterState> {
        self.resolved.get(&key)
    }

    #[cfg(test)]
    pub(crate) fn has_ready(&self) -> bool {
        self.resolved
            .values()
            .any(|state| matches!(state, RasterState::Ready(_)))
    }

    pub(crate) fn ensure_svg(
        &mut self,
        key: u64,
        svg: String,
        natural_hint: Size<f32>,
        target_width: f32,
        cx: &mut Context<HtmlState>,
    ) {
        if self.resolved.contains_key(&key) || self.in_flight.contains_key(&key) {
            return;
        }
        if let Some(cached) = RASTER_CACHE.with(|c| c.borrow_mut().get(key)) {
            self.resolved.insert(key, cached);
            return;
        }
        let renderer = cx.svg_renderer();
        let bg = cx
            .background_executor()
            .spawn(async move { rasterize_svg(&svg, natural_hint, target_width, &renderer) });
        let task = cx.spawn(async move |this, cx| {
            let result = bg.await;
            let _ = this.update(cx, |state, cx| {
                let outcome = match result {
                    Ok((image, sz)) => RasterState::Ready(RasterPicture::new(image, sz, cx)),
                    Err(err) => RasterState::Failed(err.into()),
                };
                RASTER_CACHE.with(|c| c.borrow_mut().insert(key, outcome.clone()));
                state.media.in_flight.remove(&key);
                state.media.resolved.insert(key, outcome);
                cx.notify();
            });
        });
        self.in_flight.insert(key, task);
    }

    pub(crate) fn ensure_data_image(
        &mut self,
        key: u64,
        data_uri: String,
        cx: &mut Context<HtmlState>,
    ) {
        if self.resolved.contains_key(&key) || self.in_flight.contains_key(&key) {
            return;
        }
        if let Some(cached) = RASTER_CACHE.with(|c| c.borrow_mut().get(key)) {
            self.resolved.insert(key, cached);
            return;
        }
        let renderer = cx.svg_renderer();
        let bg = cx
            .background_executor()
            .spawn(async move { decode_data_image(&data_uri, &renderer) });
        let task = cx.spawn(async move |this, cx| {
            let result = bg.await;
            let _ = this.update(cx, |state, cx| {
                let outcome = match result {
                    Ok((image, sz)) => RasterState::Ready(RasterPicture::new(image, sz, cx)),
                    Err(err) => RasterState::Failed(err.into()),
                };
                RASTER_CACHE.with(|c| c.borrow_mut().insert(key, outcome.clone()));
                state.media.in_flight.remove(&key);
                state.media.resolved.insert(key, outcome);
                cx.notify();
            });
        });
        self.in_flight.insert(key, task);
    }
}

pub(crate) struct PreparedSvg {
    pub(crate) key: u64,
    pub(crate) xml: String,
    pub(crate) natural: Size<f32>,
}

/// Preprocess an inline `<svg>` for `usvg`/`resvg`:
/// - resolves `var(--...)` custom properties
/// - replaces `currentColor` with the element's resolved foreground color
/// - ensures `xmlns="http://www.w3.org/2000/svg"` is present
/// - injects document `<style>` blocks (with variables resolved) so classes inside SVG work
/// - determines natural dimensions from `width`/`height`/`viewBox`
pub(crate) fn prepare_svg(
    raw_svg: &str,
    current_color: Hsla,
    vars: &HashMap<String, String>,
    stylesheets: &[String],
    font_family: &str,
) -> PreparedSvg {
    let mut xml = resolve_vars(raw_svg, vars);
    let cur_css = hsla_to_css(current_color);
    xml = replace_ascii_case_insensitive(&xml, "currentcolor", &cur_css);

    let natural = extract_svg_dimensions(&xml);

    // Ensure `<svg ...>` carries xmlns, default color, and explicit pixel dimensions
    // (usvg cannot rasterize percentage dimensions like `width="100%"` reliably without a viewport).
    if let Some(gt) = xml.find('>') {
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
        if !has_svg_attr(open_tag, "width") {
            additions.push_str(&format!(r#" width="{}""#, natural.width.round() as u32));
        }
        if !has_svg_attr(open_tag, "height") {
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

    // Inject resolved stylesheet + base font-family rule (including CJK fallbacks) right after `<svg ...>`.
    if let Some(gt) = xml.find('>')
        && xml.as_bytes().get(gt.saturating_sub(1)) != Some(&b'/')
    {
        let mut families = Vec::new();
        if !font_family.is_empty() {
            families.push(format!("\"{font_family}\""));
        }
        for cjk in crate::i18n::cjk_fallback_families() {
            families.push(format!("\"{cjk}\""));
        }
        for extra in [
            "Noto Sans CJK SC",
            "Noto Sans SC",
            "WenQuanYi Micro Hei",
            "PingFang SC",
            "Microsoft YaHei",
        ] {
            let quoted = format!("\"{extra}\"");
            if !families.contains(&quoted) {
                families.push(quoted);
            }
        }
        families.push("sans-serif".into());
        let font_stack = families.join(", ");
        let mut injected_css =
            format!("text, tspan {{ font-family: {font_stack}; fill: {cur_css}; }}");
        for sheet in stylesheets {
            let resolved = resolve_vars(sheet, vars);
            let resolved = replace_ascii_case_insensitive(&resolved, "currentcolor", &cur_css);
            // Only forward rules that look relevant or are concise; keep safe for XML CDATA.
            if !resolved.contains("]]>") {
                injected_css.push('\n');
                injected_css.push_str(&resolved);
            }
        }
        let style_block = format!("<style><![CDATA[{injected_css}]]></style>");
        xml.insert_str(gt + 1, &style_block);
    }

    let mut hasher = DefaultHasher::new();
    "svg".hash(&mut hasher);
    xml.hash(&mut hasher);
    PreparedSvg {
        key: hasher.finish(),
        xml,
        natural,
    }
}

pub(crate) fn data_image_key(src: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    "data-img".hash(&mut hasher);
    src.hash(&mut hasher);
    hasher.finish()
}

fn raster_scale(natural: Size<f32>, target_width: f32) -> f32 {
    if natural.width <= 0.0 || natural.height <= 0.0 {
        return gpui::SMOOTH_SVG_SCALE_FACTOR;
    }
    let upscale = if target_width > natural.width {
        (target_width / natural.width).min(4.0)
    } else {
        1.0
    };
    let mut scale = gpui::SMOOTH_SVG_SCALE_FACTOR
        * upscale
        * (MAX_DISPLAY_HEIGHT / (natural.height * upscale)).min(1.0);
    let pixels = natural.width * natural.height * scale * scale;
    if pixels > MAX_PIXELS {
        scale *= (MAX_PIXELS / pixels).sqrt();
    }
    scale.max(0.25)
}

fn rasterize_svg(
    svg: &str,
    natural_hint: Size<f32>,
    target_width: f32,
    renderer: &SvgRenderer,
) -> Result<(Arc<RenderImage>, Size<Pixels>), String> {
    let scale = raster_scale(natural_hint, target_width);
    let image = renderer
        .render_single_frame(svg.as_bytes(), scale / gpui::SMOOTH_SVG_SCALE_FACTOR)
        .map_err(|e| e.to_string())?;
    let device = image.size(0);
    let logical = size(
        px(device.width.0 as f32 / scale),
        px(device.height.0 as f32 / scale),
    );
    Ok((image, logical))
}

fn decode_data_image(
    data_uri: &str,
    renderer: &SvgRenderer,
) -> Result<(Arc<RenderImage>, Size<Pixels>), String> {
    let rest = data_uri
        .strip_prefix("data:")
        .ok_or_else(|| "not a data URI".to_string())?;
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| "malformed data URI".to_string())?;
    let meta_lower = meta.to_ascii_lowercase();
    let bytes = if meta_lower.contains(";base64") {
        base64::engine::general_purpose::STANDARD
            .decode(payload.trim().as_bytes())
            .map_err(|e| e.to_string())?
    } else {
        percent_encoding::percent_decode_str(payload).collect()
    };

    if meta_lower.starts_with("image/svg+xml") {
        let svg_str = String::from_utf8(bytes).map_err(|e| e.to_string())?;
        let natural = extract_svg_dimensions(&svg_str);
        return rasterize_svg(&svg_str, natural, natural.width, renderer);
    }

    let decoded = image::load_from_memory(&bytes)
        .map_err(|e| e.to_string())?
        .into_rgba8();
    let (w, h) = decoded.dimensions();
    // Convert RGBA -> BGRA as GPUI's `RenderImage` expects.
    let mut bgra = decoded;
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    let frame = image::Frame::new(bgra);
    let render_image = Arc::new(RenderImage::new(vec![frame]));
    Ok((render_image, size(px(w as f32), px(h as f32))))
}

/// Extract intrinsic `(width, height)` from `<svg>`'s `width`/`height`/`viewBox`.
pub(crate) fn extract_svg_dimensions(svg: &str) -> Size<f32> {
    let open = svg.find('>').map_or(svg, |gt| &svg[..gt]);
    let attr_w = read_svg_attr(open, "width").and_then(parse_svg_number);
    let attr_h = read_svg_attr(open, "height").and_then(parse_svg_number);
    let vb = read_svg_attr(open, "viewBox")
        .or_else(|| read_svg_attr(open, "viewbox"))
        .and_then(parse_viewbox);

    match (attr_w, attr_h, vb) {
        (Some(w), Some(h), _) if w > 0.0 && h > 0.0 => size(w, h),
        (Some(w), None, Some((vw, vh))) if w > 0.0 && vw > 0.0 && vh > 0.0 => {
            size(w, w * (vh / vw))
        }
        (None, Some(h), Some((vw, vh))) if h > 0.0 && vw > 0.0 && vh > 0.0 => {
            size(h * (vw / vh), h)
        }
        (_, _, Some((vw, vh))) if vw > 0.0 && vh > 0.0 => size(vw, vh),
        (Some(w), _, _) if w > 0.0 => size(w, w * 0.6),
        (_, Some(h), _) if h > 0.0 => size(h * 1.5, h),
        _ => size(480.0, 240.0),
    }
}

fn has_svg_attr(open_tag: &str, name: &str) -> bool {
    read_svg_attr(open_tag, name).is_some()
}

fn replace_pct_svg_attr(xml: &str, name: &str, px_val: u32) -> String {
    let Some(gt) = xml.find('>') else {
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
    let stripped = t.strip_suffix("px").unwrap_or(t).trim();
    stripped.parse::<f32>().ok().filter(|n| *n > 0.0)
}

fn parse_viewbox(s: &str) -> Option<(f32, f32)> {
    let nums: Vec<f32> = s
        .split(|c: char| c.is_ascii_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<f32>().ok())
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
