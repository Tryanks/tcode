use std::path::PathBuf;

use gpui::{Context, Entity, Window};
use tcode_protocol::ThreadExportFormat;

use crate::overlay::{Notification, OverlayExt as _};
use crate::store::WorkspaceStore;

pub(crate) fn prompt_thread_export<V: 'static>(
    store: Entity<WorkspaceStore>,
    session_id: String,
    title: String,
    directory: PathBuf,
    format: ThreadExportFormat,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    // The platform save panel owns destination selection and overwrite
    // confirmation; no path is written before it returns `Some`.
    let extension = match format {
        ThreadExportFormat::Jsonl => "jsonl",
        ThreadExportFormat::Markdown => "md",
    };
    let filename = format!("{}.{}", safe_filename(&title), extension);
    let receiver = cx.prompt_for_new_path(&directory, Some(&filename));
    cx.spawn_in(window, async move |this, cx| {
        let Ok(result) = receiver.await else {
            return;
        };
        match result {
            Ok(Some(destination)) => {
                let _ = this.update_in(cx, |_view, _window, cx| {
                    store.update(cx, |store, _cx| {
                        store.export_thread(session_id, destination, format);
                    });
                });
            }
            Ok(None) => {}
            Err(error) => {
                let _ = this.update_in(cx, |_view, window, cx| {
                    window.push_notification(
                        Notification::error(crate::tr!(
                            "errors.export_thread",
                            error = error.to_string()
                        )),
                        cx,
                    );
                });
            }
        }
    })
    .detach();
}

fn safe_filename(title: &str) -> String {
    let sanitized: String = title
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            character if character.is_control() => '-',
            character => character,
        })
        .collect();
    let sanitized = sanitized.trim().trim_matches('.');
    if sanitized.is_empty() {
        "thread".into()
    } else {
        sanitized.chars().take(120).collect()
    }
}
