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
                        store.apply_connection_state(tcode_client::ConnectionState::Syncing);
                    }
                    // Seed a baseline before caching the error, then replace it on reconnect.
                    ServerEvent::IndexSnapshot(IndexSnapshot {
                        sessions: vec![],
                        projects: vec![project.clone()],
                        activity: Default::default(),
                        title_generating: Default::default(),
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
                let (image, requested) = cx.update(|cx| cx.fetch_asset::<HostImage>(&key(pixels)));
                assert!(
                    requested,
                    "a host event must invalidate both success and error entries"
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
                match color {
                    Some(color) => {
                        let image = image.await.unwrap();
                        let rgba = image::load_from_memory(&image.bytes).unwrap().into_rgba8();
                        assert_eq!(rgba.dimensions(), (pixels, pixels));
                        assert_eq!(rgba.get_pixel(0, 0).0, color);
                    }
                    None => assert!(image.await.is_err()),
                }
                assert!(!cx.update(|cx| cx.fetch_asset::<HostImage>(&key(pixels)).1));
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
            crate::overlay::OverlayHost::new(view, window, cx)
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
