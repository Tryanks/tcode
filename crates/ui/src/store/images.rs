//! Host-owned image paths loaded through the query plane and GPUI's asset cache.
use gpui::{App, Asset, Global, Image, ImageCacheError, ImageSource};
use std::{path::PathBuf, sync::Arc};
use tcode_client::HostLink;
use tcode_protocol::{Query, QueryResponse};

pub(super) struct HostImages {
    pub link: Option<HostLink>,
    pub namespace: u64,
    /// Existing live-host UI fixtures pump on an OS thread. Keep its wakeups
    /// outside GPUI's deterministic scheduler; scripted fixtures stay async.
    #[cfg(test)]
    pub blocking_queries: bool,
}
impl Global for HostImages {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ImageRequest {
    File(PathBuf),
    Item {
        session_id: String,
        item_id: String,
        image_index: usize,
    },
    Thumbnail(PathBuf),
    Project {
        id: String,
        override_path: Option<PathBuf>,
        pixels: u32,
    },
}

struct HostImage;
impl Asset for HostImage {
    type Source = (u64, ImageRequest);
    type Output = Result<Arc<Image>, ImageCacheError>;
    fn load(
        (namespace, request): Self::Source,
        cx: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        let images = cx.global::<HostImages>();
        let host = images
            .link
            .clone()
            .filter(|_| images.namespace == namespace)
            .ok_or_else(|| std::io::Error::other("image belongs to a detached host"));
        #[cfg(test)]
        let blocking_queries = images.blocking_queries;
        async move {
            let query = match request {
                ImageRequest::File(path) => Query::ReadFileBytes { path },
                ImageRequest::Item {
                    session_id,
                    item_id,
                    image_index,
                } => Query::ReadItemImage {
                    session_id,
                    item_id,
                    image_index,
                },
                ImageRequest::Thumbnail(path) => Query::ReadIconImage { path },
                ImageRequest::Project { id, pixels, .. } => Query::ReadProjectIcon {
                    project_id: id,
                    pixels,
                },
            };
            let host = host?;
            #[cfg(test)]
            let result = if blocking_queries {
                futures_lite::future::block_on(host.query(query))
            } else {
                host.query(query).await
            };
            #[cfg(not(test))]
            let result = host.query(query).await;
            let bytes = match result {
                Ok(QueryResponse::FileBytes(bytes)) => bytes,
                result => {
                    return Err(std::io::Error::other(format!(
                        "host image read failed: {result:?}"
                    ))
                    .into());
                }
            };
            let format = image::guess_format(&bytes)?;
            let format = gpui::ImageFormat::from_mime_type(format.to_mime_type())
                .ok_or_else(|| std::io::Error::other("unsupported host image format"))?;
            Ok(Arc::new(Image::from_bytes(format, bytes)))
        }
    }
}

fn source(request: impl Fn(&gpui::Window) -> ImageRequest + 'static) -> ImageSource {
    ImageSource::from(move |window: &mut gpui::Window, cx: &mut App| {
        let namespace = cx.try_global::<HostImages>()?.namespace;
        match window.use_asset::<HostImage>(&(namespace, request(window)), cx)? {
            Ok(image) => image.use_render_image(window, cx).map(Ok),
            Err(error) => Some(Err(error)),
        }
    })
}

pub(crate) fn host_image(path: PathBuf) -> ImageSource {
    source(move |_| ImageRequest::File(path.clone()))
}

pub(crate) fn item_image(session_id: String, item_id: String, image_index: usize) -> ImageSource {
    source(move |_| ImageRequest::Item {
        session_id: session_id.clone(),
        item_id: item_id.clone(),
        image_index,
    })
}

/// Media a pull request names, keyed by the GitHub account that read its conversation.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MediaRequest {
    session_id: String,
    key: tcode_core::pull_request::PullRequestKey,
    account: String,
    url: String,
}

/// What the host answered for one pull request media URL.
#[derive(Clone)]
enum MediaOutcome {
    Image {
        image: Arc<Image>,
        validator: Option<String>,
        /// Unix seconds after which the host is asked again, with `validator`.
        expires_at: u64,
    },
    /// Video or audio, by MIME type.
    External(String),
    /// Not media the host reads; drawn by its URL.
    Elsewhere,
    TooLarge,
    Unavailable,
}

struct PullRequestMediaAsset;
impl Asset for PullRequestMediaAsset {
    type Source = (u64, MediaRequest);
    type Output = MediaOutcome;
    fn load(
        (namespace, request): Self::Source,
        cx: &mut App,
    ) -> impl std::future::Future<Output = Self::Output> + Send + 'static {
        let images = cx.global::<HostImages>();
        let host = images
            .link
            .clone()
            .filter(|_| images.namespace == namespace);
        #[cfg(test)]
        let blocking_queries = images.blocking_queries;
        // The copy an expired answer left: the host may answer that it is still current.
        let held = cx
            .try_global::<PullRequestMediaCache>()
            .and_then(|cache| cache.expired.get(&(namespace, request.clone())).cloned());
        async move {
            let Some(host) = host else {
                return MediaOutcome::Unavailable;
            };
            let query = Query::PullRequest {
                session_id: request.session_id,
                key: request.key,
                read: tcode_protocol::PullRequestRead::Media {
                    url: request.url,
                    validator: held.as_ref().and_then(|(_, validator)| validator.clone()),
                },
            };
            #[cfg(test)]
            let result = if blocking_queries {
                futures_lite::future::block_on(host.query(query))
            } else {
                host.query(query).await
            };
            #[cfg(not(test))]
            let result = host.query(query).await;
            use tcode_protocol::{PullRequestMedia, PullRequestReadResponse};
            let response = match result {
                Ok(QueryResponse::PullRequest { response, .. }) => *response,
                Err(error) if error.code == "pull_request_too_large" => {
                    return MediaOutcome::TooLarge;
                }
                _ => return MediaOutcome::Unavailable,
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
                        Some(format) => MediaOutcome::Image {
                            image: Arc::new(Image::from_bytes(format, bytes)),
                            validator,
                            expires_at,
                        },
                        None => MediaOutcome::Unavailable,
                    }
                }
                PullRequestReadResponse::Media(PullRequestMedia::NotModified { expires_at }) => {
                    match held {
                        Some((image, validator)) => MediaOutcome::Image {
                            image,
                            validator,
                            expires_at,
                        },
                        None => MediaOutcome::Unavailable,
                    }
                }
                PullRequestReadResponse::Media(PullRequestMedia::External { mime }) => {
                    MediaOutcome::External(mime)
                }
                PullRequestReadResponse::Media(PullRequestMedia::Unsupported) => {
                    MediaOutcome::Elsewhere
                }
                _ => MediaOutcome::Unavailable,
            }
        }
    }
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

/// The pull request media this client holds decoded, oldest first, within a byte budget.
#[derive(Default)]
struct PullRequestMediaCache {
    held: std::collections::VecDeque<((u64, MediaRequest), usize)>,
    bytes: usize,
    /// The account each host's GitHub host was last read as.
    accounts: std::collections::HashMap<(u64, String), String>,
    /// Copies whose expiry passed, drawn while the host is asked whether they are current.
    expired: std::collections::HashMap<(u64, MediaRequest), (Arc<Image>, Option<String>)>,
    /// Reads that failed, asked again on a refresh or once the conversation is read again.
    failed: std::collections::HashSet<(u64, MediaRequest)>,
}
impl Global for PullRequestMediaCache {}

const PULL_REQUEST_MEDIA_BYTES: usize = 64 * 1024 * 1024;

/// The host's answer for a media URL; an image past its expiry keeps drawing while it is read
/// again with its validator.
fn media_outcome(
    session_id: &str,
    key: &tcode_core::pull_request::PullRequestKey,
    account: &str,
    url: &str,
    window: &mut gpui::Window,
    cx: &mut App,
) -> Option<MediaOutcome> {
    let namespace = cx.try_global::<HostImages>()?.namespace;
    let request = (
        namespace,
        MediaRequest {
            session_id: session_id.to_owned(),
            key: key.clone(),
            account: account.to_owned(),
            url: url.to_owned(),
        },
    );
    let outcome = window.use_asset::<PullRequestMediaAsset>(&request, cx);
    match &outcome {
        Some(MediaOutcome::Image {
            image,
            validator,
            expires_at,
        }) => {
            hold_pull_request_media(request.clone(), image.bytes().len(), cx);
            let cache = cx.default_global::<PullRequestMediaCache>();
            if *expires_at > tcode_core::project::now_secs() {
                cache.expired.remove(&request);
            } else {
                cache
                    .expired
                    .insert(request.clone(), (image.clone(), validator.clone()));
                cx.remove_asset::<PullRequestMediaAsset>(&request);
                let _ = window.use_asset::<PullRequestMediaAsset>(&request, cx);
            }
        }
        Some(MediaOutcome::TooLarge | MediaOutcome::Unavailable) => {
            let cache = cx.default_global::<PullRequestMediaCache>();
            cache.expired.remove(&request);
            cache.failed.insert(request);
        }
        Some(_) => {}
        None => {
            let held = cx
                .try_global::<PullRequestMediaCache>()
                .and_then(|cache| cache.expired.get(&request).cloned());
            if let Some((image, validator)) = held {
                return Some(MediaOutcome::Image {
                    image,
                    validator,
                    expires_at: 0,
                });
            }
        }
    }
    outcome
}

pub(crate) fn pull_request_media_state(
    session_id: &str,
    key: &tcode_core::pull_request::PullRequestKey,
    account: &str,
    url: &str,
    window: &mut gpui::Window,
    cx: &mut App,
) -> MediaState {
    match media_outcome(session_id, key, account, url, window, cx) {
        None => MediaState::Loading,
        // An image elsewhere is drawn by its URL, as any page would draw it.
        Some(MediaOutcome::Image { .. } | MediaOutcome::Elsewhere) => MediaState::Ready,
        Some(MediaOutcome::External(mime)) => MediaState::External(mime),
        Some(MediaOutcome::TooLarge) => MediaState::TooLarge,
        Some(MediaOutcome::Unavailable) => MediaState::Unavailable,
    }
}

/// Media a pull request's conversation names, read by the host. Each account's copies are
/// its own: a conversation read as another account names another `account`, and reading
/// one drops every copy the previous account read.
pub(crate) fn pull_request_media(
    session_id: String,
    key: tcode_core::pull_request::PullRequestKey,
    account: String,
    url: String,
) -> ImageSource {
    ImageSource::from(move |window: &mut gpui::Window, cx: &mut App| {
        match media_outcome(&session_id, &key, &account, &url, window, cx)? {
            MediaOutcome::Image { image, .. } => image.use_render_image(window, cx).map(Ok),
            MediaOutcome::Elsewhere => window
                .use_asset::<gpui::ImgResourceLoader>(&gpui::Resource::Uri(url.clone().into()), cx),
            _ => Some(Err(ImageCacheError::Asset(
                "pull request media is not drawn as an image".into(),
            ))),
        }
    })
}

/// Drops the pull request's failed media reads, so the next draw asks the host again.
pub(crate) fn retry_failed_pull_request_media(
    key: &tcode_core::pull_request::PullRequestKey,
    cx: &mut App,
) {
    if !cx.has_global::<PullRequestMediaCache>() {
        return;
    }
    let cache = cx.global_mut::<PullRequestMediaCache>();
    let failed: Vec<_> = cache
        .failed
        .iter()
        .filter(|(_, request)| request.key == *key)
        .cloned()
        .collect();
    for request in &failed {
        cache.failed.remove(request);
    }
    for request in failed {
        cx.remove_asset::<PullRequestMediaAsset>(&request);
    }
}

fn hold_pull_request_media(request: (u64, MediaRequest), bytes: usize, cx: &mut App) {
    let partition = (request.0, request.1.key.host.clone());
    let account = request.1.account.clone();
    let cache = cx.default_global::<PullRequestMediaCache>();
    let mut dropped = Vec::new();
    if cache.accounts.get(&partition) != Some(&account) {
        cache.accounts.insert(partition.clone(), account.clone());
        let other = |(namespace, held): &(u64, MediaRequest)| {
            (*namespace, held.key.host.clone()) == partition && held.account != account
        };
        cache.held.retain(|(held, size)| {
            if other(held) {
                dropped.push((held.clone(), *size));
            }
            !other(held)
        });
        cache.expired.retain(|held, _| !other(held));
        cache.failed.retain(|held| !other(held));
    }
    if !cache.held.iter().any(|(held, _)| *held == request) {
        cache.held.push_back((request, bytes));
        cache.bytes += bytes;
    }
    cache.bytes -= dropped.iter().map(|(_, size)| size).sum::<usize>();
    while cache.bytes > PULL_REQUEST_MEDIA_BYTES && cache.held.len() > 1 {
        let Some(oldest) = cache.held.pop_front() else {
            break;
        };
        cache.bytes -= oldest.1;
        dropped.push(oldest);
    }
    for (request, _) in dropped {
        cx.remove_asset::<PullRequestMediaAsset>(&request);
    }
}

pub(crate) fn icon_thumbnail(path: PathBuf) -> ImageSource {
    source(move |_| ImageRequest::Thumbnail(path.clone()))
}

pub(crate) fn project_icon(
    project: &tcode_core::project::Project,
    logical_size: f32,
) -> ImageSource {
    let id = project.id.clone();
    let override_path = project.icon_path.clone();
    source(move |window| ImageRequest::Project {
        id: id.clone(),
        override_path: override_path.clone(),
        pixels: (logical_size * window.scale_factor())
            .round()
            .clamp(1., 128.) as u32,
    })
}

/// A reset can return to a previously cached default after the project config changes.
pub(crate) fn invalidate_project_icon(project: &tcode_core::project::Project, cx: &mut App) {
    if let Some(images) = cx.try_global::<HostImages>() {
        let namespace = images.namespace;
        // Each display scale has its own host-rendered raster, including cached errors.
        for pixels in 1..=128 {
            cx.remove_asset::<HostImage>(&(
                namespace,
                ImageRequest::Project {
                    id: project.id.clone(),
                    override_path: project.icon_path.clone(),
                    pixels,
                },
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{MarkdownState, MarkdownView};
    use crate::overlay::OverlayExt as _;
    use gpui::{
        AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, KeyUpEvent,
        Keystroke, ParentElement as _, Render, Styled as _, TestAppContext, Window, div, px,
    };
    use tcode_protocol::{ClientPayload, HostMessage, decode_client_line, encode_line};

    #[gpui::test]
    async fn project_icon_cache_refreshes_defaults_and_errors_after_reset_or_reconnect(
        cx: &mut TestAppContext,
    ) {
        use crate::store::{WorkspaceAttachment, WorkspaceStore};
        use tcode_core::project::Project;
        use tcode_protocol::{EventEnvelope, IndexSnapshot, ProtocolError, ServerEvent, Topic};

        cx.update(crate::theme::init);
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        let store = cx.new(|cx| {
            WorkspaceStore::new_attached(
                link.clone(),
                WorkspaceAttachment::Local,
                None,
                None,
                false,
                cx,
            )
        });
        crate::store::tests::seed_full_scope(&store, &replies, Vec::new(), cx);
        store.update(cx, |store, _| {
            // Keep navigation out of this cache test when its Index baseline arrives.
            store.selected_session_id = Some("selected".into());
        });
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            link.pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        let project = Project::from_root(PathBuf::from("/host/project"));
        let namespace = cx.update(|cx| cx.global::<HostImages>().namespace);
        let key = |pixels| {
            (
                namespace,
                ImageRequest::Project {
                    id: project.id.clone(),
                    override_path: None,
                    pixels,
                },
            )
        };
        for (reconnect, color) in [
            (None, None),
            (Some(true), Some([255, 0, 0, 255])),
            (Some(false), Some([0, 0, 255, 255])),
        ] {
            store.update(cx, |store, cx| {
                let event = if reconnect == Some(false) {
                    // Also delivered to clients that did not open the picker.
                    ServerEvent::IndexUpsertProject(project.clone())
                } else {
                    if reconnect == Some(true) {
                        store.apply_connection_state(tcode_client::ConnectionState::Syncing {
                            path: None,
                        });
                    }
                    // Seed a baseline before caching the error, then replace it on reconnect.
                    ServerEvent::IndexSnapshot(IndexSnapshot {
                        summary: Default::default(),
                        sessions: vec![],
                        projects: vec![project.clone()],
                    })
                };
                store.apply_domain_event(
                    &EventEnvelope {
                        request_id: None,
                        topic: Topic::Index,
                        event,
                    },
                    cx,
                );
            });
            for pixels in [16, 32] {
                assert!(
                    !cx.update(|cx| cx.has_asset::<HostImage>(&key(pixels))),
                    "a host event must invalidate both success and error entries"
                );
                assert!(
                    cx.update(|cx| cx.fetch_asset::<HostImage>(&key(pixels)))
                        .is_none()
                );
                cx.run_until_parked();
                let request = std::iter::from_fn(|| requests.try_recv().ok())
                    .map(|line| decode_client_line(&line).unwrap())
                    .find(|request| {
                        matches!(
                            request.payload,
                            ClientPayload::Query(Query::ReadProjectIcon { .. })
                        )
                    })
                    .expect("an uncached project icon must query the host");
                assert_eq!(
                    request.payload,
                    ClientPayload::Query(Query::ReadProjectIcon {
                        project_id: project.id.clone(),
                        pixels,
                    })
                );
                let result = match color {
                    Some(color) => {
                        let mut png = std::io::Cursor::new(Vec::new());
                        image::RgbaImage::from_pixel(pixels, pixels, image::Rgba(color))
                            .write_to(&mut png, image::ImageFormat::Png)
                            .unwrap();
                        Ok(QueryResponse::FileBytes(png.into_inner()))
                    }
                    None => Err(ProtocolError {
                        code: "not_found".into(),
                        message: "no default icon".into(),
                    }),
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
                let image = cx
                    .update(|cx| cx.fetch_asset::<HostImage>(&key(pixels)))
                    .expect("the host reply completes the cached load");
                match color {
                    Some(color) => {
                        let image = image.unwrap();
                        let rgba = image::load_from_memory(&image.bytes).unwrap().into_rgba8();
                        assert_eq!(rgba.dimensions(), (pixels, pixels));
                        assert_eq!(rgba.get_pixel(0, 0).0, color);
                    }
                    None => assert!(image.is_err()),
                }
                assert!(cx.update(|cx| cx.has_asset::<HostImage>(&key(pixels))));
            }
        }
    }

    struct ImageMessage {
        markdown: Entity<MarkdownState>,
        cwd: PathBuf,
    }

    impl Render for ImageMessage {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("image-message")
                .tab_index(0)
                .w(px(320.))
                .child(MarkdownView::new(&self.markdown).base_dir(self.cwd.clone()))
        }
    }

    #[gpui::test]
    fn markdown_images_and_badge_previews_read_files_from_the_host(cx: &mut TestAppContext) {
        cx.update(crate::theme::init);
        cx.update(crate::markdown::init);
        let (to_host, requests) = async_channel::unbounded();
        let (replies, from_host) = async_channel::unbounded();
        let link = HostLink::new(to_host, from_host);
        cx.update(|cx| {
            cx.set_global(HostImages {
                link: Some(link.clone()),
                namespace: 1,
                blocking_queries: false,
            });
        });
        let executor = cx.background_executor.clone();
        let _pump = cx.background_executor.spawn(async move {
            link.pump_with_timer(|| executor.timer(std::time::Duration::from_millis(25)))
                .await;
        });
        // These paths exist only on the scripted host, never on the viewing client.
        let cwd = std::env::current_dir().unwrap().join("host-only-images");
        let mut message = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| ImageMessage {
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
                    file_url_path,
                ),
            ];
            for (uri, expected_path) in cases {
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
                    ClientPayload::Query(Query::ReadFileBytes {
                        path: expected_path
                    }),
                    "{markdown}",
                );
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
        let request = decode_client_line(
            &requests
                .try_recv()
                .expect("clicking the image badge must read its host image"),
        )
        .unwrap();
        assert_eq!(
            request.payload,
            ClientPayload::Query(Query::ReadFileBytes {
                path: cwd.join("preview.png")
            })
        );
        assert!(
            cx.opened_url().is_none(),
            "host image badges must open in the app"
        );
    }
}
