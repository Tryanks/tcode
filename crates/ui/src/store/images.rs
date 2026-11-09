//! Every image Tcode draws: what it reads from the host (previews, icons, pull request media)
//! and by URL, decoded here, and the pictures it makes itself ([`Owned`]), held within a budget.
//!
//! gpui-base has no bounded image cache. GPUI's asset memo keeps every read until it is
//! removed and each window's atlas keeps every tile until it is dropped, and its `ImageCache`
//! reaches neither an `ImageSource::Custom` source nor, under its `image_cache` element,
//! gpui-fast's rebuild of a nested view on its own (`Window::rebuild_here`). So Tcode reads,
//! decodes and lets go of its images here, until gpui-base offers a cache.
//!
//! gpui-fast draws a frame again without the code that painted it: a view drawn from last frame
//! replays its sprites, a clean window presents its last frame again, a scroll layer composites
//! what it painted. A sprite carries its atlas tile, and one drawn after its tile left the atlas
//! shows nothing or another image's pixels. What gpui-fast keeps for exactly as long as such a
//! frame may still be shown is the element state of what it drew, so every element that asks for
//! an image keeps a `Claim` there and every element that draws one a `Lease`: a decode leaves
//! memory and every atlas only after the frame that let go of its last lease, and only what no
//! element claims is let go of to keep within the budget.
use gpui::{
    App, AsyncApp, BorrowAppContext as _, ElementId, EntityId, Global, ImageCacheError,
    ImageSource, RenderImage, SharedString, SharedUri, Task, Window, WindowId,
};
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    future::Future,
    hash::{Hash as _, Hasher as _},
    path::PathBuf,
    pin::Pin,
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};
use tcode_client::HostLink;
use tcode_protocol::{Query, QueryResponse};
#[cfg(target_family = "wasm")]
use web_time::Instant;

/// The host a picture is read from. Several hosts' pictures are on screen at once, so a caller
/// names its host, through its store, and nothing here assumes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ImageHost(u64);

impl ImageHost {
    /// No host: what it asks for reads as unreached, and draws a surface's fallback.
    pub(crate) const NONE: Self = Self(0);
}

/// The link each open host's pictures are read through.
#[derive(Default)]
pub(crate) struct HostImages {
    links: HashMap<ImageHost, HostLink>,
}
impl Global for HostImages {}

impl HostImages {
    pub(crate) fn open(link: HostLink, cx: &mut App) -> ImageHost {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let host = ImageHost(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        if !cx.has_global::<Self>() {
            cx.set_global(Self::default());
        }
        cx.update_global::<Self, _>(|images, _| {
            images.links.insert(host, link);
        });
        host
    }

    pub(crate) fn close(host: ImageHost, cx: &mut App) {
        if cx.has_global::<Self>() {
            cx.update_global::<Self, _>(|images, _| {
                images.links.remove(&host);
            });
        }
    }

    pub(crate) fn link(host: ImageHost, cx: &App) -> Option<HostLink> {
        cx.try_global::<Self>()?.links.get(&host).cloned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ImageRequest {
    Preview {
        path: PathBuf,
        pixels: u32,
        cover: bool,
    },
    ItemPreview {
        session_id: String,
        item_id: String,
        image_index: usize,
        width: u32,
        height: u32,
    },
    Thumbnail(PathBuf),
    Project {
        id: String,
        override_path: Option<PathBuf>,
        pixels: u32,
    },
}

/// Media a pull request names, keyed by the GitHub account that read its conversation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MediaRequest {
    session_id: String,
    key: tcode_core::pull_request::PullRequestKey,
    account: String,
    url: String,
}

/// Where an image is read from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Source {
    Host(ImageHost, ImageRequest),
    Media(ImageHost, MediaRequest),
    Url(SharedUri),
}

impl Source {
    /// The host whose answer this is, for a source a host reads.
    fn host(&self) -> Option<ImageHost> {
        match self {
            Self::Host(host, _) | Self::Media(host, _) => Some(*host),
            Self::Url(_) => None,
        }
    }
}

/// What a source answered.
#[derive(Clone)]
enum Answer {
    Image(Rc<Decoded>),
    /// Video or audio a pull request names, by MIME type.
    External(String),
    /// Pull request media the host does not read; drawn by its URL.
    Elsewhere,
    TooLarge,
    /// The answer that there is nothing to draw, such as no preview for this source.
    Unavailable(SharedString),
    /// The read never reached the host's answer; it is asked again when next drawn.
    Unreached(Instant),
}

/// When pull request media expires, and the validator to read it again with.
#[derive(Clone)]
struct Expiry {
    at: u64,
    validator: Option<String>,
}

struct Entry {
    answer: Option<Answer>,
    read: Option<Task<()>>,
    /// Shared with every element that asks for it.
    hold: Rc<Hold>,
    /// The views drawing it, drawn again when the read lands.
    waiting: HashSet<EntityId>,
    /// Read again when next drawn, and drawn as it is until that read lands.
    stale: bool,
    expiry: Option<Expiry>,
}

impl Entry {
    fn new(after_frame: Rc<AfterFrame>) -> Self {
        Self {
            answer: None,
            read: None,
            hold: Rc::new(Hold {
                released: Cell::new(after_frame.tick()),
                after_frame,
            }),
            waiting: HashSet::new(),
            stale: false,
            expiry: None,
        }
    }

    /// Whether an element of a frame some window may still show asks for it or draws it.
    fn held(&self) -> bool {
        Rc::strong_count(&self.hold) > 1
            || matches!(&self.answer, Some(Answer::Image(decoded)) if Rc::strong_count(decoded) > 1)
    }
}

struct Hold {
    /// When the last element asking for it let go of it.
    released: Cell<u64>,
    after_frame: Rc<AfterFrame>,
}

/// Kept in the element state of every element that asks a source for its image, from its first
/// ask on, so neither what it draws nor a read landing for it is let go while it is drawn.
struct Claim(Rc<Hold>);

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.released.set(self.0.after_frame.tick());
        self.0.after_frame.schedule();
    }
}

/// What a read found, off the main thread.
enum Read {
    Image {
        image: Arc<RenderImage>,
        expiry: Option<Expiry>,
    },
    /// The copy the read's validator names is current until then.
    NotModified(u64),
    External(String),
    Elsewhere,
    TooLarge,
    Unavailable(SharedString),
    Unreached(Instant),
}

type Reading = Pin<Box<dyn Future<Output = Read> + Send>>;

/// Query errors raised before the host answered.
const UNREACHED: [&str; 3] = ["timeout", "transport_closed", "disconnected"];

/// A read that did not reach the host is asked again only when drawn this long after it failed,
/// so a link that fails at once cannot turn each redraw into another read.
const RETRY_UNREACHED: Duration = Duration::from_secs(5);

/// The largest side, in physical pixels, the host renders a preview at.
pub(crate) const MAX_PREVIEW_PIXELS: u32 = 1024;

/// What decoded images may cost, each counted once in memory and once in every window's atlas
/// that drew it. A drawn image stays whatever it costs; past this, those no longer drawn go,
/// released longest ago first.
// A full 5K screen of pictures costs ~118 MB; the rest keeps the last few image threads.
#[cfg(not(any(target_os = "ios", target_os = "android", target_family = "wasm")))]
const BUDGET: usize = 256 << 20;
// A phone's full screen of pictures costs ~24 MB; the rest keeps the last thread or two.
#[cfg(any(target_os = "ios", target_os = "android", target_family = "wasm"))]
const BUDGET: usize = 96 << 20;

/// A decoded image. Once nothing holds it, it leaves every window's atlas.
struct Decoded {
    image: Arc<RenderImage>,
    bytes: usize,
    /// The windows whose atlas has a copy.
    windows: RefCell<Vec<WindowId>>,
    after_frame: Rc<AfterFrame>,
}

impl Decoded {
    fn new(image: Arc<RenderImage>, after_frame: Rc<AfterFrame>) -> Self {
        let bytes = (0..image.frame_count())
            .filter_map(|frame| image.as_bytes(frame))
            .map(<[u8]>::len)
            .sum();
        Self {
            image,
            bytes,
            windows: RefCell::default(),
            after_frame,
        }
    }

    fn cost(&self) -> usize {
        self.bytes * (1 + self.windows.borrow().len())
    }
}

impl Drop for Decoded {
    fn drop(&mut self) {
        self.after_frame
            .retired
            .borrow_mut()
            .push(self.image.clone());
        self.after_frame.schedule();
    }
}

/// Kept in the element state of every element that draws a decoded image, which may be one its
/// source no longer answers, or one Tcode made itself.
struct Lease(Rc<Decoded>);

/// Sweeps after the frame that let go of an image. Leases are let go while a window draws, when
/// that window is out of the app's list and so out of `App::drop_image`'s reach.
struct AfterFrame {
    app: AsyncApp,
    /// Gone with the app's globals, after which nothing is swept.
    alive: Weak<()>,
    scheduled: Cell<bool>,
    clock: Cell<u64>,
    /// Images nothing holds anymore, whose tiles the sweep drops from every window.
    retired: RefCell<Vec<Arc<RenderImage>>>,
}

impl AfterFrame {
    fn tick(&self) -> u64 {
        let tick = self.clock.get() + 1;
        self.clock.set(tick);
        tick
    }

    fn schedule(&self) {
        if self.alive.strong_count() == 0 || self.scheduled.replace(true) {
            return;
        }
        let (app, alive) = (self.app.clone(), self.alive.clone());
        // `AsyncApp::update` panics once the app is gone, which can happen before the task runs.
        self.app
            .foreground_executor()
            .spawn(async move {
                if alive.upgrade().is_some() {
                    app.update(sweep);
                }
            })
            .detach();
    }
}

struct Images {
    entries: HashMap<Source, Entry>,
    /// The account each host's GitHub host was last read as.
    accounts: HashMap<(ImageHost, String), String>,
    after_frame: Rc<AfterFrame>,
    _alive: Rc<()>,
}
impl Global for Images {}

fn images(cx: &mut App) -> &mut Images {
    if !cx.has_global::<Images>() {
        let alive = Rc::new(());
        let after_frame = Rc::new(AfterFrame {
            app: cx.to_async(),
            alive: Rc::downgrade(&alive),
            scheduled: Cell::new(false),
            clock: Cell::new(0),
            retired: RefCell::default(),
        });
        cx.set_global(Images {
            entries: HashMap::new(),
            accounts: HashMap::new(),
            after_frame,
            _alive: alive,
        });
    }
    cx.global_mut::<Images>()
}

/// What `source` answers the view drawing it, read when it has no answer yet or is due to be
/// read again; the view is drawn again when the read lands.
fn answer(source: &Source, window: &mut Window, cx: &mut App) -> Option<Answer> {
    let view = window.current_view();
    let state = images(cx);
    let after_frame = state.after_frame.clone();
    let entry = state
        .entries
        .entry(source.clone())
        .or_insert_with(|| Entry::new(after_frame));
    let due = entry.stale
        || match &entry.answer {
            None => true,
            Some(Answer::Unreached(failed)) => failed.elapsed() >= RETRY_UNREACHED,
            Some(_) => entry
                .expiry
                .as_ref()
                .is_some_and(|expiry| expiry.at <= tcode_core::project::now_secs()),
        };
    let read = due && entry.read.is_none();
    if entry.answer.is_none() || read {
        entry.waiting.insert(view);
    }
    let answer = entry.answer.clone();
    let hold = entry.hold.clone();
    let mut key = std::hash::DefaultHasher::new();
    source.hash(&mut key);
    let key = ElementId::NamedInteger("image-claim".into(), key.finish());
    window.with_global_id(key, |key, window| {
        window.with_element_state(key, |claim: Option<Claim>, _| {
            let claim = claim.filter(|claim| Rc::ptr_eq(&claim.0, &hold));
            ((), claim.unwrap_or_else(|| Claim(hold)))
        })
    });
    if read {
        start_read(source, cx);
    }
    answer
}

/// Reads `source` (pull request media with the validator of the copy it holds), in place of any
/// read in flight.
fn start_read(source: &Source, cx: &mut App) {
    let validator = images(cx)
        .entries
        .get(source)
        .and_then(|entry| entry.expiry.as_ref())
        .and_then(|expiry| expiry.validator.clone());
    let reading = reading(source, validator, cx);
    let landing = source.clone();
    let task = cx.spawn(async move |cx| {
        let read = cx.background_executor().spawn(reading).await;
        cx.update(|cx| landed(&landing, read, cx));
    });
    if let Some(entry) = images(cx).entries.get_mut(source) {
        entry.read = Some(task);
    }
}

fn reading(source: &Source, validator: Option<String>, cx: &mut App) -> Reading {
    let renderer = cx.svg_renderer();
    match source.clone() {
        Source::Host(host, request) => {
            let host = HostImages::link(host, cx);
            Box::pin(async move {
                let query = match request {
                    ImageRequest::Preview {
                        path,
                        pixels,
                        cover,
                    } => Query::ReadImagePreview {
                        path,
                        pixels,
                        cover,
                    },
                    ImageRequest::ItemPreview {
                        session_id,
                        item_id,
                        image_index,
                        width,
                        height,
                    } => Query::ReadItemImagePreview {
                        session_id,
                        item_id,
                        image_index,
                        width,
                        height,
                    },
                    ImageRequest::Thumbnail(path) => Query::ReadIconImage { path },
                    ImageRequest::Project { id, pixels, .. } => Query::ReadProjectIcon {
                        project_id: id,
                        pixels,
                    },
                };
                let Some(host) = host else {
                    return Read::Unreached(Instant::now());
                };
                match host.query(query).await {
                    Ok(QueryResponse::FileBytes(bytes)) => {
                        let format = image::guess_format(&bytes).ok().and_then(|format| {
                            gpui::ImageFormat::from_mime_type(format.to_mime_type())
                        });
                        match format {
                            Some(format) => decode(format, bytes, renderer, None),
                            None => Read::Unavailable("unsupported host image format".into()),
                        }
                    }
                    Err(error) if UNREACHED.contains(&error.code.as_str()) => {
                        Read::Unreached(Instant::now())
                    }
                    Err(error) => Read::Unavailable(error.message.into()),
                    Ok(response) => Read::Unavailable(format!("unexpected {response:?}").into()),
                }
            })
        }
        Source::Media(host, request) => {
            let host = HostImages::link(host, cx);
            Box::pin(async move {
                let unavailable = || Read::Unavailable("pull request media is unavailable".into());
                let Some(host) = host else {
                    return unavailable();
                };
                let query = Query::PullRequest {
                    session_id: request.session_id,
                    key: request.key,
                    read: tcode_protocol::PullRequestRead::Media {
                        url: request.url,
                        validator,
                    },
                };
                use tcode_protocol::{PullRequestMedia, PullRequestReadResponse};
                let response = match host.query(query).await {
                    Ok(QueryResponse::PullRequest { response, .. }) => *response,
                    Err(error) if error.code == "pull_request_too_large" => return Read::TooLarge,
                    _ => return unavailable(),
                };
                match response {
                    PullRequestReadResponse::Media(PullRequestMedia::Image {
                        bytes,
                        mime,
                        validator,
                        expires_at,
                    }) => {
                        // The host decided the type; an SVG is drawn as an image only.
                        match gpui::ImageFormat::from_mime_type(&mime) {
                            Some(format) => decode(
                                format,
                                bytes,
                                renderer,
                                Some(Expiry {
                                    at: expires_at,
                                    validator,
                                }),
                            ),
                            None => unavailable(),
                        }
                    }
                    PullRequestReadResponse::Media(PullRequestMedia::NotModified {
                        expires_at,
                    }) => Read::NotModified(expires_at),
                    PullRequestReadResponse::Media(PullRequestMedia::External { mime }) => {
                        Read::External(mime)
                    }
                    PullRequestReadResponse::Media(PullRequestMedia::Unsupported) => {
                        Read::Elsewhere
                    }
                    _ => unavailable(),
                }
            })
        }
        Source::Url(uri) => {
            let load = <gpui::ImgResourceLoader as gpui::Asset>::load(gpui::Resource::Uri(uri), cx);
            Box::pin(async move {
                match load.await {
                    Ok(image) => Read::Image {
                        image,
                        expiry: None,
                    },
                    Err(error) => Read::Unavailable(error.to_string().into()),
                }
            })
        }
    }
}

fn decode(
    format: gpui::ImageFormat,
    bytes: Vec<u8>,
    renderer: gpui::SvgRenderer,
    expiry: Option<Expiry>,
) -> Read {
    match gpui::Image::from_bytes(format, bytes).to_image_data(renderer) {
        Ok(image) => Read::Image { image, expiry },
        Err(error) => Read::Unavailable(error.to_string().into()),
    }
}

fn landed(source: &Source, read: Read, cx: &mut App) {
    let images = images(cx);
    let after_frame = images.after_frame.clone();
    let Some(entry) = images.entries.get_mut(source) else {
        return;
    };
    // The read landing is this task's own: detached, not cancelled from inside its run.
    if let Some(task) = entry.read.take() {
        task.detach();
    }
    entry.stale = false;
    let answer = match read {
        Read::Image { image, expiry } => {
            entry.expiry = expiry;
            Answer::Image(Rc::new(Decoded::new(image, after_frame.clone())))
        }
        Read::NotModified(at) => {
            if let (Some(Answer::Image(_)), Some(expiry)) = (&entry.answer, &mut entry.expiry) {
                // The same decode draws on: nothing drawn changes.
                expiry.at = at;
                entry.waiting.clear();
                return;
            }
            Answer::Unavailable("pull request media is unavailable".into())
        }
        Read::External(mime) => Answer::External(mime),
        Read::Elsewhere => Answer::Elsewhere,
        Read::TooLarge => Answer::TooLarge,
        Read::Unavailable(reason) => Answer::Unavailable(reason),
        Read::Unreached(failed) => Answer::Unreached(failed),
    };
    if !matches!(answer, Answer::Image(_)) {
        entry.expiry = None;
    }
    // A decode this replaces stays with the elements still drawing it, until they let go.
    entry.answer = Some(answer);
    let waiting = std::mem::take(&mut entry.waiting);
    for view in waiting {
        cx.notify(view);
    }
    after_frame.schedule();
}

/// `decoded`'s image, leased to the element drawing it for as long as gpui-fast keeps that
/// element's state.
///
/// A row a virtual list's scroll layer holds drops its states when it leaves the viewport while
/// the layer still holds what it painted; gpui-fast renders that row again before it shows
/// (`fast/layers/lists.rs`, `sort_rows`), so a tile the row no longer leases is never sampled on
/// screen.
fn lease(decoded: &Rc<Decoded>, window: &mut Window) -> Arc<RenderImage> {
    let id = window.window_handle().window_id();
    if !decoded.windows.borrow().contains(&id) {
        decoded.windows.borrow_mut().push(id);
    }
    // Keyed by the image, so the `img`s of one id namespace drawing it share one lease and those
    // drawing others never take its place.
    let key = ElementId::NamedInteger("image-lease".into(), decoded.image.id.0 as u64);
    window.with_global_id(key, |key, window| {
        window.with_element_state(key, |lease: Option<Lease>, _| {
            let lease = lease.filter(|lease| Rc::ptr_eq(&lease.0, decoded));
            ((), lease.unwrap_or_else(|| Lease(decoded.clone())))
        })
    });
    decoded.image.clone()
}

fn drawn(answer: Answer, window: &mut Window) -> Result<Arc<RenderImage>, ImageCacheError> {
    match answer {
        Answer::Image(decoded) => Ok(lease(&decoded, window)),
        Answer::Unavailable(reason) => Err(ImageCacheError::Asset(reason)),
        Answer::Unreached(_) => Err(ImageCacheError::Asset(
            "the host image read did not reach the host".into(),
        )),
        Answer::External(_) | Answer::Elsewhere | Answer::TooLarge => Err(ImageCacheError::Asset(
            "pull request media is not drawn as an image".into(),
        )),
    }
}

fn draw(
    source: Source,
    window: &mut Window,
    cx: &mut App,
) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
    let answer = answer(&source, window, cx)?;
    Some(drawn(answer, window))
}

/// Lets go of what no element asks for: past the budget, the decodes released longest ago, and at
/// once answers without a picture and those of a host this client no longer reads; then every
/// retired image's atlas tiles.
fn sweep(cx: &mut App) {
    let windows: Vec<WindowId> = cx
        .windows()
        .iter()
        .map(|window| window.window_id())
        .collect();
    let open: HashSet<ImageHost> = cx
        .try_global::<HostImages>()
        .map(|images| images.links.keys().copied().collect())
        .unwrap_or_default();
    let images = images(cx);
    let after_frame = images.after_frame.clone();
    let mut total = 0;
    let mut unheld = Vec::new();
    for (source, entry) in &images.entries {
        let cost = match &entry.answer {
            Some(Answer::Image(decoded)) => {
                decoded
                    .windows
                    .borrow_mut()
                    .retain(|window| windows.contains(window));
                decoded.cost()
            }
            _ => 0,
        };
        total += cost;
        // An answer without a picture is kept only while something asks for it.
        let pictureless = entry.answer.is_some() && cost == 0;
        let gone = source.host().is_some_and(|held| !open.contains(&held));
        if !entry.held() && (cost > 0 || pictureless || gone) {
            let keep = !(pictureless || gone);
            unheld.push((keep, entry.hold.released.get(), cost, source.clone()));
        }
    }
    unheld.sort_by_key(|(keep, released, ..)| (*keep, *released));
    for (keep, _, cost, source) in unheld {
        if keep && total <= BUDGET {
            break;
        }
        images.entries.remove(&source);
        total -= cost;
    }
    let retired = std::mem::take(&mut *after_frame.retired.borrow_mut());
    for image in retired {
        cx.drop_image(image, None);
    }
    after_frame.scheduled.set(false);
}

/// A picture Tcode makes and owns itself, outside the budget: it leaves memory and every atlas
/// once its owners and every frame drawing it have let go of it.
#[derive(Clone)]
pub(crate) struct Owned(Rc<Decoded>);

impl Owned {
    pub(crate) fn new(image: Arc<RenderImage>, cx: &mut App) -> Self {
        let after_frame = images(cx).after_frame.clone();
        Self(Rc::new(Decoded::new(image, after_frame)))
    }

    pub(crate) fn image(&self) -> &Arc<RenderImage> {
        &self.0.image
    }

    /// Draws the picture, leasing it to the elements that do.
    pub(crate) fn source(&self) -> ImageSource {
        let decoded = self.0.clone();
        ImageSource::from(move |window: &mut Window, _: &mut App| Some(Ok(lease(&decoded, window))))
    }
}

/// Draws a host image; an `img` shows its fallback for every read without an image.
fn source(host: ImageHost, request: impl Fn(&Window) -> ImageRequest + 'static) -> ImageSource {
    ImageSource::from(move |window: &mut Window, cx: &mut App| {
        draw(Source::Host(host, request(window)), window, cx)
    })
}

/// The host's preview of `path` for a `logical_pixels` square, covering it when `cover` is set.
/// It never falls back to the original: without a preview, the tile draws its fallback and
/// opening it reads the original.
pub(crate) fn image_preview(
    host: ImageHost,
    path: PathBuf,
    logical_pixels: f32,
    cover: bool,
) -> ImageSource {
    source(host, move |window| ImageRequest::Preview {
        path: path.clone(),
        pixels: preview_pixels(logical_pixels, window),
        cover,
    })
}

fn preview_pixels(logical: f32, window: &Window) -> u32 {
    (logical * window.scale_factor())
        .ceil()
        .clamp(1., MAX_PREVIEW_PIXELS as f32) as u32
}

pub(crate) fn item_image_preview(
    host: ImageHost,
    session_id: String,
    item_id: String,
    image_index: usize,
    width: f32,
    height: f32,
) -> ImageSource {
    source(host, move |window| ImageRequest::ItemPreview {
        session_id: session_id.clone(),
        item_id: item_id.clone(),
        image_index,
        width: preview_pixels(width, window),
        height: preview_pixels(height, window),
    })
}

/// A picture by its URL, read as a browser reads it.
pub(crate) fn remote_image(uri: SharedUri) -> ImageSource {
    ImageSource::from(move |window: &mut Window, cx: &mut App| {
        draw(Source::Url(uri.clone()), window, cx)
    })
}

/// Where one pull request media read stands for a view drawing it.
pub(crate) enum MediaState {
    Loading,
    Ready,
    /// Video or audio, by MIME type.
    External(String),
    TooLarge,
    Unavailable,
}

/// The host's answer for a media URL; an image past its expiry keeps drawing while it is read
/// again with its validator.
fn media_answer(
    host: ImageHost,
    session_id: &str,
    key: &tcode_core::pull_request::PullRequestKey,
    account: &str,
    url: &str,
    window: &mut Window,
    cx: &mut App,
) -> Option<Answer> {
    let request = MediaRequest {
        session_id: session_id.to_owned(),
        key: key.clone(),
        account: account.to_owned(),
        url: url.to_owned(),
    };
    read_as(host, &request, cx);
    answer(&Source::Media(host, request), window, cx)
}

/// Each account's copies are its own: reading a host's conversations as another account forgets
/// every copy the previous one read.
fn read_as(host: ImageHost, request: &MediaRequest, cx: &mut App) {
    let images = images(cx);
    let partition = (host, request.key.host.clone());
    if images.accounts.get(&partition) == Some(&request.account) {
        return;
    }
    images.accounts.insert(partition, request.account.clone());
    images.entries.retain(|source, _| {
        !matches!(source, Source::Media(held_host, held)
            if *held_host == host
                && held.key.host == request.key.host
                && held.account != request.account)
    });
}

pub(crate) fn pull_request_media_state(
    host: ImageHost,
    session_id: &str,
    key: &tcode_core::pull_request::PullRequestKey,
    account: &str,
    url: &str,
    window: &mut Window,
    cx: &mut App,
) -> MediaState {
    match media_answer(host, session_id, key, account, url, window, cx) {
        None => MediaState::Loading,
        // An image elsewhere is drawn by its URL, as any page would draw it.
        Some(Answer::Image(_) | Answer::Elsewhere) => MediaState::Ready,
        Some(Answer::External(mime)) => MediaState::External(mime),
        Some(Answer::TooLarge) => MediaState::TooLarge,
        Some(Answer::Unavailable(_) | Answer::Unreached(_)) => MediaState::Unavailable,
    }
}

/// Media a pull request's conversation names, read by the host. Each account's copies are
/// its own: a conversation read as another account names another `account`, and reading
/// one drops every copy the previous account read.
pub(crate) fn pull_request_media(
    host: ImageHost,
    session_id: String,
    key: tcode_core::pull_request::PullRequestKey,
    account: String,
    url: String,
) -> ImageSource {
    ImageSource::from(move |window: &mut Window, cx: &mut App| {
        match media_answer(host, &session_id, &key, &account, &url, window, cx)? {
            Answer::Elsewhere => draw(Source::Url(url.clone().into()), window, cx),
            answer => Some(drawn(answer, window)),
        }
    })
}

/// Drops the pull request's failed media reads, so the next draw asks the host again.
pub(crate) fn retry_failed_pull_request_media(
    key: &tcode_core::pull_request::PullRequestKey,
    cx: &mut App,
) {
    if !cx.has_global::<Images>() {
        return;
    }
    images(cx).entries.retain(|source, entry| {
        !(matches!(source, Source::Media(_, request) if request.key == *key)
            && matches!(
                entry.answer,
                Some(Answer::TooLarge | Answer::Unavailable(_))
            ))
    });
}

pub(crate) fn icon_thumbnail(host: ImageHost, path: PathBuf) -> ImageSource {
    source(host, move |_| ImageRequest::Thumbnail(path.clone()))
}

pub(crate) fn project_icon(
    host: ImageHost,
    project: &tcode_core::project::Project,
    logical_size: f32,
) -> ImageSource {
    let id = project.id.clone();
    let override_path = project.icon_path.clone();
    source(host, move |window| ImageRequest::Project {
        id: id.clone(),
        override_path: override_path.clone(),
        pixels: (logical_size * window.scale_factor())
            .round()
            .clamp(1., 128.) as u32,
    })
}

/// Why a project's icons are read again.
pub(crate) enum IconReset {
    /// The host changed the project: a reset can return to a previously read default after its
    /// config changes, and a read in flight may have been answered before the change, so it is
    /// read again now, for the views waiting on it.
    Changed,
    /// A fresh index baseline, which may follow changes this client missed. A read in flight was
    /// sent on the current connection, since losing the connection fails every read in flight
    /// (`HostLink::set_connection_state`), and stands: reading it again would ask the host twice
    /// for what one read answers.
    Baseline,
}

/// Every display scale's icon of `project`, error included, is read again when next drawn.
pub(crate) fn invalidate_project_icon(
    host: ImageHost,
    project: &tcode_core::project::Project,
    reset: IconReset,
    cx: &mut App,
) {
    if !cx.has_global::<Images>() {
        return;
    }
    let mut in_flight = Vec::new();
    for (source, entry) in &mut images(cx).entries {
        if let Source::Host(
            held,
            ImageRequest::Project {
                id, override_path, ..
            },
        ) = source
            && *held == host
            && *id == project.id
            && *override_path == project.icon_path
        {
            entry.stale = true;
            if entry.read.is_some() && matches!(reset, IconReset::Changed) {
                in_flight.push(source.clone());
            }
        }
    }
    for source in in_flight {
        start_read(&source, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{MarkdownState, MarkdownView};
    use crate::overlay::OverlayExt as _;
    use crate::test_frames::draw;
    use gpui::{
        AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, KeyUpEvent,
        Keystroke, ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
        Window, div, img, px,
    };
    use std::sync::Mutex;
    use tcode_protocol::{
        ClientPayload, HostMessage, ProtocolError, PullRequestMedia, PullRequestReadResponse,
        decode_client_line, encode_line,
    };

    /// The side of the pictures that press on the budget.
    const SIDE: u32 = 2048;

    /// What one such picture costs once drawn in one window: its decode and its atlas copy.
    const COST: usize = 2 * (SIDE * SIDE * 4) as usize;

    fn bmp(side: u32) -> Vec<u8> {
        let mut bmp = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(side, side, image::Rgba([0, 0, 255, 255]))
            .write_to(&mut bmp, image::ImageFormat::Bmp)
            .unwrap();
        bmp.into_inner()
    }

    /// The host end of a scripted link.
    struct Host {
        image: ImageHost,
        requests: async_channel::Receiver<String>,
        replies: async_channel::Sender<String>,
        _pump: gpui::Task<()>,
    }

    impl Host {
        fn attach(cx: &mut TestAppContext) -> Self {
            let (to_host, requests) = async_channel::unbounded();
            let (replies, from_host) = async_channel::unbounded();
            let link = HostLink::new(to_host, from_host);
            let image = cx.update(|cx| HostImages::open(link.clone(), cx));
            let executor = cx.background_executor.clone();
            let pump = cx.background_executor.spawn(async move {
                link.pump_with_timer(|delay| executor.timer(delay)).await;
            });
            Self {
                image,
                requests,
                replies,
                _pump: pump,
            }
        }

        /// Answers every query asked since with `answer`, returning the queries.
        fn answer(
            &self,
            answer: impl Fn(&Query) -> Result<QueryResponse, ProtocolError>,
        ) -> Vec<Query> {
            let mut asked = Vec::new();
            while let Ok(line) = self.requests.try_recv() {
                let request = decode_client_line(&line).unwrap();
                let ClientPayload::Query(query) = request.payload else {
                    continue;
                };
                let result = answer(&query);
                self.replies
                    .send_blocking(
                        encode_line(&HostMessage::QueryResult {
                            id: request.id,
                            result,
                        })
                        .unwrap(),
                    )
                    .unwrap();
                asked.push(query);
            }
            asked
        }
    }

    fn media_image(
        bytes: Vec<u8>,
        validator: Option<&str>,
        expires_at: u64,
    ) -> Result<QueryResponse, ProtocolError> {
        media(PullRequestMedia::Image {
            bytes,
            mime: "image/bmp".into(),
            validator: validator.map(Into::into),
            expires_at,
        })
    }

    fn media(media: PullRequestMedia) -> Result<QueryResponse, ProtocolError> {
        Ok(QueryResponse::PullRequest {
            response: Box::new(PullRequestReadResponse::Media(media)),
            expires_at: u64::MAX,
        })
    }

    fn pull_request() -> tcode_core::pull_request::PullRequestKey {
        tcode_core::pull_request::PullRequestKey {
            host: "github.com".into(),
            repository: "owner/repo".into(),
            number: 1,
        }
    }

    /// Where Tcode reads a picture from.
    #[derive(Clone, Copy)]
    enum Kind {
        Preview,
        Media,
        Url,
    }

    impl Kind {
        /// The `index`th picture read this way, named `…/{index}.bmp`.
        fn source(self, host: ImageHost, index: usize) -> ImageSource {
            match self {
                Kind::Preview => {
                    image_preview(host, format!("/host/{index}.bmp").into(), 64., false)
                }
                Kind::Media => pull_request_media(
                    host,
                    "session".into(),
                    pull_request(),
                    "reader".into(),
                    format!("https://github.com/owner/repo/assets/{index}.bmp"),
                ),
                Kind::Url => remote_image(format!("https://example.com/{index}.bmp").into()),
            }
        }
    }

    /// The index a `…/{index}.bmp` name was given.
    fn index(name: &str) -> usize {
        let name = name.rsplit('/').next().unwrap();
        name.strip_suffix(".bmp").unwrap().parse().unwrap()
    }

    /// A host and a web that answer every read with one picture, recording which were read.
    struct Script {
        host: Host,
        fetched: Arc<Mutex<Vec<String>>>,
        picture: Vec<u8>,
    }

    impl Script {
        fn new(picture: Vec<u8>, cx: &mut TestAppContext) -> Self {
            let host = Host::attach(cx);
            let fetched = Arc::new(Mutex::new(Vec::new()));
            let client = gpui::http_client::FakeHttpClient::create({
                let (fetched, picture) = (fetched.clone(), picture.clone());
                move |request| {
                    fetched.lock().unwrap().push(request.uri().to_string());
                    let picture = picture.clone();
                    async move {
                        Ok(gpui::http_client::Response::builder()
                            .status(200)
                            .body(picture.into())?)
                    }
                }
            });
            cx.update(|cx| cx.set_http_client(client));
            Self {
                host,
                fetched,
                picture,
            }
        }

        /// Answers the reads asked since, returning the index of each picture read.
        fn answer(&self) -> Vec<usize> {
            let mut read: Vec<usize> = self
                .host
                .answer(|query| match query {
                    Query::ReadImagePreview { .. } => {
                        Ok(QueryResponse::FileBytes(self.picture.clone()))
                    }
                    Query::PullRequest { .. } => media_image(self.picture.clone(), None, u64::MAX),
                    other => panic!("unexpected read {other:?}"),
                })
                .iter()
                .map(|query| match query {
                    Query::ReadImagePreview { path, .. } => index(&path.to_string_lossy()),
                    Query::PullRequest {
                        read: tcode_protocol::PullRequestRead::Media { url, .. },
                        ..
                    } => index(url),
                    other => panic!("unexpected read {other:?}"),
                })
                .collect();
            read.extend(
                self.fetched
                    .lock()
                    .unwrap()
                    .drain(..)
                    .map(|url| index(&url)),
            );
            read
        }
    }

    /// Draws one picture at a time, as a tile does, noting what it decoded to.
    struct Shelf {
        source: Option<ImageSource>,
        drawn: Rc<RefCell<Option<Arc<RenderImage>>>>,
        renders: Rc<Cell<usize>>,
    }

    impl Shelf {
        fn new(source: Option<ImageSource>) -> Self {
            Self {
                source,
                drawn: Rc::default(),
                renders: Rc::default(),
            }
        }
    }

    impl Render for Shelf {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.renders.set(self.renders.get() + 1);
            let shelf = div().size(px(64.));
            let Some(source) = self.source.clone() else {
                *self.drawn.borrow_mut() = None;
                return shelf;
            };
            let ImageSource::Custom(load) = &source else {
                unreachable!("Tcode's pictures are read through `store::images`");
            };
            *self.drawn.borrow_mut() = match load(window, cx) {
                Some(Ok(image)) => Some(image),
                _ => None,
            };
            shelf.child(img(source).size_full())
        }
    }

    fn show(shelf: &Entity<Shelf>, source: ImageSource, cx: &mut VisualTestContext) {
        shelf.update(cx, |shelf, cx| {
            shelf.source = Some(source);
            cx.notify();
        });
    }

    /// Draws a full frame, answers the reads it asked and draws the frame that shows them,
    /// returning which pictures were read.
    fn settle(script: &Script, cx: &mut VisualTestContext) -> Vec<usize> {
        settle_with(script, cx, draw)
    }

    /// As `settle`, with frames drawn as a display has them drawn: views nothing notified are
    /// drawn from last frame (`test_frames::draw` refreshes, which builds every view again).
    fn settle_reusing(script: &Script, cx: &mut VisualTestContext) -> Vec<usize> {
        settle_with(script, cx, |cx| {
            cx.run_until_parked();
            cx.update(|window, cx| {
                let _ = window.draw(cx);
            });
        })
    }

    fn settle_with(
        script: &Script,
        cx: &mut VisualTestContext,
        frame: impl Fn(&mut VisualTestContext),
    ) -> Vec<usize> {
        frame(cx);
        cx.run_until_parked();
        let read = script.answer();
        cx.run_until_parked();
        frame(cx);
        // What the frame let go of is swept after it.
        cx.run_until_parked();
        read
    }

    fn atlas_holds(image: &RenderImage, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, _| window.has_image_atlas_entry(image))
    }

    /// Pictures drawn one after another, each let go of when the next is drawn: within the
    /// budget every one stays decoded and in the atlas; past it, the one let go of longest ago
    /// leaves both while the one let go of last stays and is drawn again without a read; the
    /// one that left is read once more when drawn again.
    fn pictures_no_longer_drawn_leave_oldest_first_past_the_budget(
        kind: Kind,
        cx: &mut TestAppContext,
    ) {
        let script = Script::new(bmp(SIDE), cx);
        let shelf = Shelf::new(None);
        let drawn = shelf.drawn.clone();
        let (shelf, cx) = cx.add_window_view(|_, _| shelf);
        let count = BUDGET / COST + 1;
        let (mut first, mut last_released) = (None, None);
        for index in 0..count {
            show(&shelf, kind.source(script.host.image, index), cx);
            assert_eq!(settle(&script, cx), [index], "each picture is read once");
            let image = drawn.borrow_mut().take().expect("the picture is drawn");
            assert!(atlas_holds(&image, cx));
            if index == 0 {
                first = Some(image);
            } else if index == count - 2 {
                last_released = Some(image);
            }
            let first = first.as_ref().unwrap();
            if index + 1 < count {
                assert!(
                    Arc::strong_count(first) > 1 && atlas_holds(first, cx),
                    "within the budget, a picture no longer drawn stays ({index})"
                );
            }
        }
        let (first, last_released) = (first.unwrap(), last_released.unwrap());
        assert_eq!(
            Arc::strong_count(&first),
            1,
            "past the budget, the decode let go of longest ago is freed"
        );
        assert!(!atlas_holds(&first, cx), "and its tile leaves the atlas");
        assert!(
            Arc::strong_count(&last_released) > 1 && atlas_holds(&last_released, cx),
            "the picture let go of last stays"
        );

        show(&shelf, kind.source(script.host.image, count - 2), cx);
        assert!(
            settle(&script, cx).is_empty(),
            "a picture still held is drawn again without a read"
        );
        let again = drawn.borrow_mut().take().expect("it is drawn at once");
        assert!(Arc::ptr_eq(&again, &last_released));
        drop((again, last_released));

        show(&shelf, kind.source(script.host.image, 0), cx);
        assert_eq!(
            settle(&script, cx),
            [0],
            "a picture that left is read again when drawn again"
        );
        assert!(drawn.borrow().is_some(), "and drawn");
    }

    #[gpui::test]
    fn host_previews_no_longer_drawn_leave_oldest_first_past_the_budget(cx: &mut TestAppContext) {
        pictures_no_longer_drawn_leave_oldest_first_past_the_budget(Kind::Preview, cx);
    }

    #[gpui::test]
    fn pull_request_media_no_longer_drawn_leaves_oldest_first_past_the_budget(
        cx: &mut TestAppContext,
    ) {
        pictures_no_longer_drawn_leave_oldest_first_past_the_budget(Kind::Media, cx);
    }

    #[gpui::test]
    fn pictures_by_url_no_longer_drawn_leave_oldest_first_past_the_budget(cx: &mut TestAppContext) {
        pictures_no_longer_drawn_leave_oldest_first_past_the_budget(Kind::Url, cx);
    }

    struct Pair {
        still: Entity<Shelf>,
        churn: Entity<Shelf>,
    }

    impl Render for Pair {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .child(self.still.clone())
                .child(self.churn.clone())
        }
    }

    /// A view that is drawn from last frame, its picture's code never running, keeps that
    /// picture decoded and in the atlas, unread again, while the pictures drawn beside it press
    /// far past the budget and those no longer drawn leave.
    #[gpui::test]
    fn a_picture_drawn_from_last_frame_outlasts_pressure_past_the_budget(cx: &mut TestAppContext) {
        let script = Script::new(bmp(SIDE), cx);
        let still = Shelf::new(Some(Kind::Preview.source(script.host.image, 0)));
        let (still_drawn, still_renders) = (still.drawn.clone(), still.renders.clone());
        let churn = Shelf::new(None);
        let churn_drawn = churn.drawn.clone();
        let (pair, cx) = cx.add_window_view(|_, cx| Pair {
            still: cx.new(|_| still),
            churn: cx.new(|_| churn),
        });
        assert_eq!(settle_reusing(&script, cx), [0]);
        let still = still_drawn
            .borrow_mut()
            .take()
            .expect("the still picture is drawn");
        let churn = pair.read_with(cx, |pair, _| pair.churn.clone());
        let renders = still_renders.get();
        let mut first_churned = None;
        for index in 1..=BUDGET / COST + 2 {
            show(&churn, Kind::Preview.source(script.host.image, index), cx);
            assert_eq!(
                settle_reusing(&script, cx),
                [index],
                "only the new picture is read"
            );
            let image = churn_drawn.borrow_mut().take().expect("it is drawn");
            first_churned.get_or_insert(image);
        }
        assert_eq!(
            still_renders.get(),
            renders,
            "the still view was drawn from last frame, not built again"
        );
        assert!(atlas_holds(&still, cx), "the still picture keeps its tile");
        assert!(Arc::strong_count(&still) > 1, "and its decode");
        let first_churned = first_churned.unwrap();
        assert_eq!(
            Arc::strong_count(&first_churned),
            1,
            "a picture no longer drawn left for it"
        );
        assert!(!atlas_holds(&first_churned, cx));
    }

    /// Pull request media past its expiry keeps drawing while the host is asked again with its
    /// validator; a copy the host says is current is drawn on, not decoded again; new bytes
    /// replace it, and the old picture leaves memory and the atlas once no frame draws it.
    #[gpui::test]
    fn pull_request_media_past_its_expiry_draws_while_it_is_read_again(cx: &mut TestAppContext) {
        let host = Host::attach(cx);
        let shelf = Shelf::new(Some(Kind::Media.source(host.image, 0)));
        let drawn = shelf.drawn.clone();
        let (_, cx) = cx.add_window_view(|_, _| shelf);
        let validators = |asked: Vec<Query>| -> Vec<Option<String>> {
            asked
                .into_iter()
                .map(|query| match query {
                    Query::PullRequest {
                        read: tcode_protocol::PullRequestRead::Media { validator, .. },
                        ..
                    } => validator,
                    other => panic!("unexpected read {other:?}"),
                })
                .collect()
        };
        // Every assertion on what is drawn is made on a frame drawn while a read is in flight:
        // an answer that lands notifies the shelf, and a test window draws a notified view as
        // the update ends (`App::flush_effects` in test mode), so after one lands the picture
        // drawn is already its answer.
        let frame = |cx: &mut VisualTestContext| {
            draw(cx);
            cx.run_until_parked();
        };
        let drawing = |picture: &Arc<RenderImage>| {
            drawn
                .borrow()
                .as_ref()
                .is_some_and(|drawn| Arc::ptr_eq(drawn, picture))
        };

        // Read once, already past its expiry.
        frame(cx);
        let asked = host.answer(|_| media_image(bmp(4), Some("first"), 1));
        assert_eq!(validators(asked), [None]);
        cx.run_until_parked();
        // Drawn, and so read again with its validator.
        frame(cx);
        let first = drawn.borrow().clone().expect("the picture is drawn");
        let asked = host.answer(|_| media(PullRequestMedia::NotModified { expires_at: 1 }));
        assert_eq!(validators(asked), [Some("first".to_string())]);
        cx.run_until_parked();
        // Still current, and still past its expiry: drawn on while it is read again.
        frame(cx);
        assert!(
            drawing(&first),
            "a copy the host says is current is drawn on, not decoded again"
        );
        let asked = host.answer(|_| media_image(bmp(4), Some("second"), u64::MAX));
        assert_eq!(validators(asked), [Some("first".to_string())]);
        cx.run_until_parked();
        frame(cx);
        let second = drawn.borrow_mut().take().expect("the new picture is drawn");
        assert!(!Arc::ptr_eq(&second, &first), "new bytes are a new picture");
        assert!(atlas_holds(&second, cx));
        assert_eq!(
            Arc::strong_count(&first),
            1,
            "the picture they replaced leaves memory once no frame draws it"
        );
        assert!(!atlas_holds(&first, cx), "and the atlas");
        frame(cx);
        assert!(
            host.answer(|_| panic!("a current copy is not read again"))
                .is_empty(),
            "a copy within its expiry is not read again"
        );
    }

    /// Each open host's pictures are read through its own link, so a second host opening takes
    /// nothing from the first, and a host closing takes nothing from the one still open.
    #[gpui::test]
    fn open_hosts_read_their_own_pictures_and_outlast_each_other(cx: &mut TestAppContext) {
        let first = Host::attach(cx);
        let second = Host::attach(cx);
        let picture = bmp(SIDE);
        let reply = |query: &Query| match query {
            Query::ReadImagePreview { .. } => Ok(QueryResponse::FileBytes(picture.clone())),
            other => panic!("unexpected read {other:?}"),
        };
        let read = |asked: Vec<Query>| -> Vec<usize> {
            asked
                .iter()
                .map(|query| match query {
                    Query::ReadImagePreview { path, .. } => index(&path.to_string_lossy()),
                    other => panic!("unexpected read {other:?}"),
                })
                .collect()
        };
        let shelf = Shelf::new(Some(Kind::Preview.source(first.image, 0)));
        let drawn = shelf.drawn.clone();
        let (shelf, cx) = cx.add_window_view(|_, _| shelf);

        draw(cx);
        cx.run_until_parked();
        assert_eq!(read(first.answer(reply)), [0]);
        assert_eq!(read(second.answer(reply)), [] as [usize; 0]);
        cx.run_until_parked();
        draw(cx);
        assert!(drawn.borrow().is_some(), "the first host's picture draws");

        show(&shelf, Kind::Preview.source(second.image, 1), cx);
        draw(cx);
        cx.run_until_parked();
        assert_eq!(read(first.answer(reply)), [] as [usize; 0]);
        assert_eq!(read(second.answer(reply)), [1]);
        cx.run_until_parked();
        draw(cx);
        assert!(drawn.borrow().is_some(), "the second host's picture draws");

        cx.update(|_, cx| HostImages::close(first.image, cx));
        show(&shelf, Kind::Preview.source(second.image, 2), cx);
        draw(cx);
        cx.run_until_parked();
        assert_eq!(read(second.answer(reply)), [2], "the open host still reads");
        cx.run_until_parked();
        draw(cx);
        assert!(drawn.borrow().is_some(), "and its picture still draws");

        show(&shelf, Kind::Preview.source(first.image, 3), cx);
        draw(cx);
        cx.run_until_parked();
        assert_eq!(read(first.answer(reply)), [] as [usize; 0]);
        assert!(drawn.borrow().is_none(), "a closed host reads nothing");
    }

    /// What a reader answered a frame.
    type Drawn = Option<Result<Arc<RenderImage>, ImageCacheError>>;

    /// Draws a project's icon at two display sizes, noting what each decoded to.
    struct ProjectIcons {
        host: ImageHost,
        project: tcode_core::project::Project,
        drawn: Rc<RefCell<Vec<Drawn>>>,
    }

    impl Render for ProjectIcons {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let mut icons = div().flex();
            let mut drawn = Vec::new();
            for size in [16., 32.] {
                let source = project_icon(self.host, &self.project, size);
                let ImageSource::Custom(load) = &source else {
                    unreachable!("project icons are read through `store::images`");
                };
                drawn.push(load(window, cx));
                icons = icons.child(img(source).size(px(size)));
            }
            *self.drawn.borrow_mut() = drawn;
            icons
        }
    }

    #[gpui::test]
    async fn project_icons_are_read_again_after_a_reset_or_reconnect(cx: &mut TestAppContext) {
        use crate::store::{WorkspaceAttachment, WorkspaceStore};
        use tcode_core::project::Project;
        use tcode_protocol::{EventEnvelope, IndexSnapshot, ServerEvent, Topic};

        cx.update(crate::theme::init);
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(link.clone(), WorkspaceAttachment::Local, None, None, cx)
        });
        crate::store::tests::seed_full_scope(&store, &replies, Vec::new(), cx);
        // Keep navigation out of this test when its Index baseline arrives.
        store.update(cx, |store, cx| store.show_alone("selected".into(), cx));
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            link.pump_with_timer(|delay| executor.timer(delay)).await;
        });
        let project = Project::from_root(PathBuf::from("/host/project"));
        let drawn = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view({
            let (project, drawn) = (project.clone(), drawn.clone());
            let host = store.read_with(cx, |store, _| store.image_host());
            move |_, _| ProjectIcons {
                host,
                project,
                drawn,
            }
        });
        let scale = cx.update(|window, _| window.scale_factor());
        let pixels = [16., 32.].map(|size: f32| (size * scale).round() as u32);
        let mut previous: Option<Vec<Drawn>> = None;
        // The host answers a color, or an error by its code: "not_found" is its answer,
        // "timeout" never reached it.
        for (reconnect, answer) in [
            (None, Err("not_found")),
            (Some(true), Err("timeout")),
            (Some(false), Ok([255, 0, 0, 255])),
            (Some(true), Ok([0, 0, 255, 255])),
        ] {
            store.update(cx, |store, cx| {
                let event = if reconnect == Some(false) {
                    // Also delivered to clients that did not open the picker.
                    ServerEvent::IndexUpsertProject(project.clone())
                } else {
                    if reconnect == Some(true) {
                        for state in [
                            tcode_client::ConnectionState::Reconnecting {
                                attempt: 1,
                                reason: None,
                            },
                            tcode_client::ConnectionState::Syncing { path: None },
                        ] {
                            store.host.set_connection_state(state.clone());
                            store.apply_connection_state(state);
                        }
                        store.receive(
                            EventEnvelope {
                                request_id: store.host.subscription_request_id(&Topic::Scope),
                                topic: Topic::Scope,
                                event: ServerEvent::ScopeSnapshot(tcode_protocol::Scope::Full),
                            },
                            cx,
                        );
                    }
                    // Seed a baseline before reading the error, then replace it on reconnect.
                    ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions: vec![],
                        projects: vec![project.clone()],
                    })
                };
                store.receive(
                    EventEnvelope {
                        request_id: None,
                        topic: Topic::Index,
                        event,
                    },
                    cx,
                );
            });
            draw(cx);
            cx.run_until_parked();
            if let Some(previous) = &previous {
                let shown = drawn.borrow();
                for (shown, previous) in shown.iter().zip(previous) {
                    let same = match (shown, previous) {
                        (Some(Ok(shown)), Some(Ok(previous))) => Arc::ptr_eq(shown, previous),
                        (Some(Err(_)), Some(Err(_))) => true,
                        _ => false,
                    };
                    assert!(
                        same,
                        "an icon read again draws as it was until the read lands"
                    );
                }
            }
            let mut asked: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok())
                .map(|line| decode_client_line(&line).unwrap())
                .filter_map(|request| match request.payload {
                    ClientPayload::Query(Query::ReadProjectIcon { project_id, pixels }) => {
                        Some((request.id, project_id, pixels))
                    }
                    _ => None,
                })
                .collect();
            asked.sort_by_key(|(_, _, pixels)| *pixels);
            assert_eq!(
                asked
                    .iter()
                    .map(|(_, id, pixels)| (id.clone(), *pixels))
                    .collect::<Vec<_>>(),
                pixels.map(|pixels| (project.id.clone(), pixels)),
                "a project event has every drawn icon read again, whatever it last answered"
            );
            for (id, _, pixels) in asked {
                let result = match answer {
                    Ok(color) => {
                        let mut png = std::io::Cursor::new(Vec::new());
                        image::RgbaImage::from_pixel(pixels, pixels, image::Rgba(color))
                            .write_to(&mut png, image::ImageFormat::Png)
                            .unwrap();
                        Ok(QueryResponse::FileBytes(png.into_inner()))
                    }
                    Err(code) => Err(ProtocolError {
                        code: code.into(),
                        message: "no icon".into(),
                    }),
                };
                replies
                    .send_blocking(encode_line(&HostMessage::QueryResult { id, result }).unwrap())
                    .unwrap();
            }
            cx.run_until_parked();
            draw(cx);
            for (shown, pixels) in drawn.borrow().iter().zip(pixels) {
                match (answer, shown) {
                    (Ok([red, green, blue, alpha]), Some(Ok(image))) => {
                        let size = image.size(0);
                        assert_eq!(
                            (size.width.0, size.height.0),
                            (pixels as i32, pixels as i32)
                        );
                        // Decoded pictures are BGRA.
                        assert_eq!(image.as_bytes(0).unwrap()[..4], [blue, green, red, alpha]);
                    }
                    (Err(_), Some(Err(_))) => {}
                    _ => panic!("the icon draws the host's answer"),
                }
            }
            previous = Some(drawn.borrow().clone());
        }
    }

    struct ImageMessage {
        host: ImageHost,
        markdown: Entity<MarkdownState>,
        cwd: PathBuf,
    }

    impl Render for ImageMessage {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().id("image-message").tab_index(0).w(px(320.)).child(
                MarkdownView::new(&self.markdown)
                    .image_host(self.host)
                    .base_dir(self.cwd.clone()),
            )
        }
    }

    #[gpui::test]
    fn markdown_images_draw_host_previews_and_open_host_originals(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let host = cx.update(|cx| HostImages::open(link.clone(), cx));
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            link.pump_with_timer(|delay| executor.timer(delay)).await;
        });
        // These paths exist only on the scripted host, never on the viewing client.
        let cwd = std::env::current_dir().unwrap().join("host-only-images");
        let mut message = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| ImageMessage {
                host,
                markdown: cx.new(|cx| MarkdownState::new("", cx)),
                cwd: cwd.clone(),
            });
            message = Some(view.clone());
            gpui_base::Root::new(view, window, cx)
        });
        let view = message.unwrap();
        for inline in [false, true] {
            let suffix = if inline { "inline" } else { "block" };
            let absolute = cwd.join(format!("absolute-{suffix}.png"));
            let relative = format!("relative-{suffix}.png");
            let file_url_path = cwd.join(format!("file image-{suffix}.png"));
            let cases = [
                (absolute.display().to_string(), absolute),
                (relative.clone(), cwd.join(relative)),
                (
                    url::Url::from_file_path(&file_url_path).unwrap().into(),
                    file_url_path.clone(),
                ),
            ];
            for (case, (uri, expected_path)) in cases.into_iter().enumerate() {
                let has_preview = case != 1;
                let mut markdown = format!("![sample](<{uri}>)");
                if inline {
                    markdown = format!("Before {markdown} after");
                }
                view.update(cx, |view, cx| {
                    view.markdown
                        .update(cx, |state, cx| state.set_text(&markdown, cx));
                });
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                cx.run_until_parked();
                let request = decode_client_line(
                    &requests
                        .try_recv()
                        .expect("Markdown image must query its host"),
                )
                .unwrap();
                assert_eq!(
                    request.payload,
                    ClientPayload::Query(Query::ReadImagePreview {
                        path: expected_path,
                        pixels: MAX_PREVIEW_PIXELS,
                        cover: false,
                    }),
                    "{markdown}",
                );
                let result = if has_preview {
                    Ok(QueryResponse::FileBytes(
                        include_bytes!("../../../../assets/icons/app/tcode.png").to_vec(),
                    ))
                } else {
                    Err(tcode_protocol::ProtocolError {
                        code: "preview_unavailable".into(),
                        message: "no preview".into(),
                    })
                };
                replies
                    .send_blocking(
                        encode_line(&HostMessage::QueryResult {
                            id: request.id,
                            result,
                        })
                        .unwrap(),
                    )
                    .unwrap();
                cx.run_until_parked();
                cx.update(|window, cx| {
                    let _ = window.draw(cx);
                });
                cx.run_until_parked();
                assert!(
                    requests.is_empty(),
                    "drawing an image without a preview must not read its original"
                );
            }

            cx.update(|window, cx| {
                window.blur(cx);
                window.focus_next(cx);
            });
            cx.simulate_keystrokes("tab");
            let image_focus = cx.update(|window, cx| {
                _ = window.draw(cx);
                window
                    .focused(cx)
                    .expect("the displayed image is a tab stop")
            });
            cx.simulate_keystrokes("shift-tab");
            cx.update(|window, _| assert!(!image_focus.is_focused(window)));
            cx.simulate_keystrokes("tab");
            cx.update(|window, _| assert!(image_focus.is_focused(window)));
            for key in ["enter", "space"] {
                cx.simulate_keystrokes(key);
                cx.simulate_event(KeyUpEvent {
                    keystroke: Keystroke::parse(key).unwrap(),
                });
                cx.run_until_parked();
                cx.update(|window, _| {
                    assert!(
                        !image_focus.is_focused(window),
                        "{key} must move focus into the image lightbox"
                    );
                });
                {
                    let request = decode_client_line(
                        &requests
                            .try_recv()
                            .expect("the lightbox must read the original"),
                    )
                    .unwrap();
                    assert_eq!(
                        request.payload,
                        ClientPayload::Query(Query::ReadFileBytes {
                            path: file_url_path.clone(),
                            max_bytes: None
                        })
                    );
                    replies
                        .send_blocking(
                            encode_line(&HostMessage::QueryResult {
                                id: request.id,
                                result: Ok(QueryResponse::FileBytes(
                                    include_bytes!("../../../../assets/icons/app/tcode.png")
                                        .to_vec(),
                                )),
                            })
                            .unwrap(),
                        )
                        .unwrap();
                    cx.run_until_parked();
                }
                for navigation in ["tab", "shift-tab"] {
                    cx.simulate_keystrokes(navigation);
                    cx.update(|window, cx| {
                        assert!(
                            gpui_base::active_focus_trap(window, cx)
                                .expect("the lightbox traps focus")
                                .contains_focused(window, cx),
                            "{navigation} must keep focus inside the lightbox"
                        );
                    });
                }
                cx.update(|window, cx| {
                    window.close_dialog(cx);
                    assert!(
                        image_focus.is_focused(window),
                        "closing the lightbox must restore image focus"
                    );
                    _ = window.draw(cx);
                });
            }
        }

        view.update(cx, |view, cx| {
            view.markdown.update(cx, |state, cx| {
                state.set_text("[Screenshot](preview.png)", cx);
            });
        });
        cx.update(|window, cx| {
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
        assert!(
            requests.is_empty(),
            "a badge loads its image only when opened"
        );
        cx.simulate_click(gpui::point(px(40.), px(14.)), gpui::Modifiers::default());
        cx.run_until_parked();
        let mut preview_requested = false;
        loop {
            let request = decode_client_line(
                &requests
                    .try_recv()
                    .expect("opening the badge reads the original"),
            )
            .unwrap();
            if let ClientPayload::Query(Query::ReadImagePreview {
                path,
                pixels,
                cover,
            }) = request.payload
            {
                assert!(
                    !preview_requested,
                    "the lightbox requests its fallback preview once"
                );
                preview_requested = true;
                assert_eq!(path, cwd.join("preview.png"));
                assert_eq!(pixels, MAX_PREVIEW_PIXELS);
                assert!(!cover);
                replies
                    .send_blocking(
                        encode_line(&HostMessage::QueryResult {
                            id: request.id,
                            result: Ok(QueryResponse::FileBytes(
                                include_bytes!("../../../../assets/icons/app/tcode.png").to_vec(),
                            )),
                        })
                        .unwrap(),
                    )
                    .unwrap();
                cx.run_until_parked();
                continue;
            }
            assert_eq!(
                request.payload,
                ClientPayload::Query(Query::ReadFileBytes {
                    path: cwd.join("preview.png"),
                    max_bytes: None
                })
            );
            break;
        }
        assert!(
            cx.opened_url().is_none(),
            "host image badges must open in the app"
        );
    }
}
