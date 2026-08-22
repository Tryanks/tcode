use std::io::Write as _;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use gpui::{AsyncApp, Entity, WindowHandle, px, size};
use tcode_ui::{AppShell, overlay::OverlayHost};

const STEP_DELAY: Duration = Duration::from_millis(20);
const RAPID_DELAY: Duration = Duration::from_millis(5);
const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(120);
const KEYS: [&str; 6] = [
    "preview-smoke-0",
    "preview-smoke-1",
    "preview-smoke-2",
    "preview-smoke-3",
    "preview-smoke-4",
    "preview-smoke-5",
];

fn log_line(message: &str) {
    eprintln!("{message}");
    let _ = std::io::stderr().flush();
}

pub struct Watchdog {
    phase: Arc<Mutex<&'static str>>,
    finished: Arc<AtomicBool>,
}

impl Watchdog {
    pub fn start() -> Self {
        let phase = Arc::new(Mutex::new("startup"));
        let finished = Arc::new(AtomicBool::new(false));
        let watchdog_phase = phase.clone();
        let watchdog_finished = finished.clone();
        std::thread::spawn(move || {
            std::thread::sleep(WATCHDOG_TIMEOUT);
            if !watchdog_finished.load(Ordering::SeqCst) {
                let phase = *watchdog_phase
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                log_line(&format!("preview-smoke: TIMEOUT phase {phase}"));
                std::process::exit(124);
            }
        });
        Self { phase, finished }
    }

    fn start_phase(&self, phase: &'static str) {
        *self.phase.lock().unwrap_or_else(|error| error.into_inner()) = phase;
        log_line(&format!("preview-smoke: phase {phase} start"));
    }

    fn finish_phase(&self, phase: &'static str) {
        log_line(&format!("preview-smoke: phase {phase} ok"));
    }

    fn pass(&self) {
        log_line("preview-smoke: PASS");
        self.finished.store(true, Ordering::SeqCst);
    }
}

async fn yield_for(cx: &AsyncApp, delay: Duration) {
    cx.background_executor().timer(delay).await;
}

pub async fn run(
    watchdog: Watchdog,
    shell: Entity<AppShell>,
    window: WindowHandle<OverlayHost>,
    cx: &mut AsyncApp,
) {
    watchdog.start_phase("create-first");
    window
        .update(cx, |_, window, cx| {
            shell.update(cx, |shell, cx| {
                shell.preview_smoke_create(KEYS[0], "about:blank", window, cx)
            })
        })
        .expect("preview smoke window closed during create-first")
        .expect("native preview webview creation failed");
    yield_for(cx, STEP_DELAY).await;
    watchdog.finish_phase("create-first");

    watchdog.start_phase("show-hide-churn");
    for _ in 0..20 {
        shell.update(cx, |shell, cx| shell.preview_smoke_set_visible(None, cx));
        yield_for(cx, RAPID_DELAY).await;
        shell.update(cx, |shell, cx| {
            shell.preview_smoke_set_visible(Some(KEYS[0]), cx)
        });
        yield_for(cx, RAPID_DELAY).await;
    }
    watchdog.finish_phase("show-hide-churn");

    watchdog.start_phase("multi-create");
    for key in &KEYS[1..] {
        window
            .update(cx, |_, window, cx| {
                shell.update(cx, |shell, cx| {
                    shell.preview_smoke_create(key, "about:blank", window, cx)
                })
            })
            .expect("preview smoke window closed during multi-create")
            .expect("native preview webview creation failed");
        yield_for(cx, STEP_DELAY).await;
    }
    watchdog.finish_phase("multi-create");

    watchdog.start_phase("rapid-switch");
    for ix in 0..30 {
        shell.update(cx, |shell, cx| {
            shell.preview_smoke_set_visible(Some(KEYS[ix % KEYS.len()]), cx)
        });
        yield_for(cx, RAPID_DELAY).await;
    }
    watchdog.finish_phase("rapid-switch");

    watchdog.start_phase("resize-churn");
    for (width, height) in [(1100., 720.), (940., 640.), (1280., 820.), (1000., 700.)] {
        window
            .update(cx, |_, window, _| {
                window.resize(size(px(width), px(height)))
            })
            .expect("preview smoke window closed during resize-churn");
        yield_for(cx, STEP_DELAY).await;
    }
    watchdog.finish_phase("resize-churn");

    watchdog.start_phase("navigate-all");
    for (ix, key) in KEYS.iter().enumerate() {
        let url = format!("about:blank#preview-smoke-{ix}");
        window
            .update(cx, |_, window, cx| {
                shell.update(cx, |shell, cx| {
                    shell.preview_smoke_create(key, &url, window, cx)
                })
            })
            .expect("preview smoke window closed during navigate-all")
            .expect("native preview navigation failed");
        yield_for(cx, STEP_DELAY).await;
    }
    watchdog.finish_phase("navigate-all");

    watchdog.start_phase("drop-one");
    shell.update(cx, |shell, cx| {
        shell.preview_smoke_set_visible(Some(KEYS[1]), cx);
        shell.preview_smoke_drop(KEYS[0], cx);
    });
    yield_for(cx, STEP_DELAY).await;
    watchdog.finish_phase("drop-one");

    watchdog.start_phase("drop-during-create");
    let close_window = window;
    window
        .update(cx, |_, window, cx| {
            shell.update(cx, |shell, cx| {
                cx.defer(move |cx| {
                    let _ = close_window.update(cx, |_, window, _| window.remove_window());
                });
                shell.preview_smoke_create(
                    "preview-smoke-drop-during-create",
                    "about:blank#drop-during-create",
                    window,
                    cx,
                )
            })
        })
        .expect("preview smoke window closed before drop-during-create")
        .expect("native preview webview creation failed");
    drop(shell);
    yield_for(cx, STEP_DELAY).await;
    watchdog.finish_phase("drop-during-create");

    watchdog.start_phase("quit");
    watchdog.finish_phase("quit");
    watchdog.pass();
    cx.update(|cx| cx.quit());
}
