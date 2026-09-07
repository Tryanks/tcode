//! One bootstrap for every client.
//!
//! A platform entry point does three things and then hands over: build its
//! `ClientHost`, describe its window seam, and call [`run_shell`]. Everything
//! after that — fonts, theme, markdown, keybindings, the window state seed, the
//! overlay host and the shell itself — is the same code everywhere.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, AppContext as _, Entity, KeyBinding, SharedString, WindowHandle, WindowOptions};
use tcode_client::host::ClientHost;

use crate::overlay::OverlayHost;
use crate::remote::{AttachmentTarget, ClientAttachment};
use crate::shell::{AppShell, ShellSetup, TogglePalette};
use crate::theme;
use crate::window_seam::WindowSeam;
use crate::window_state::WindowState;

/// The embedded palette every client renders with.
pub const THEME_JSON: &str = include_str!("../../../themes/tcode.json");

/// The palette with its translucent canvas flattened to solid RGB, for a client
/// whose window is opaque. Over a native backdrop those colors composite with
/// the material behind them; over black they just look muddy.
pub fn flattened_theme_json() -> String {
    let flattened = THEME_JSON
        .replace("#F2F4F7C7", "#F2F4F7")
        .replace("#15171CC7", "#15171C");
    debug_assert_ne!(flattened, THEME_JSON, "canvas colors moved; update flatten");
    flattened
}

/// Where a client should attach at launch: the host it used last, if that
/// record still exists. A client with no such record opens on the hosts list.
pub fn last_host_target(host: &dyn ClientHost) -> Option<AttachmentTarget> {
    let id = host.last_host_id()?;
    host.load_hosts()
        .into_iter()
        .find(|saved| saved.host_id == id)
        .map(AttachmentTarget::Remote)
}

pub struct ShellOptions {
    pub window: WindowOptions,
    /// The title the window carries until an attachment names one.
    pub title: SharedString,
    pub fonts: Vec<Cow<'static, [u8]>>,
    /// The palette this client renders with. Clients whose window is opaque
    /// hand over a theme with the translucent canvas already flattened.
    pub theme_json: Cow<'static, str>,
    /// Whether this client should bring itself to the front on launch.
    pub activate: bool,
    pub setup: ShellSetup,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            window: WindowOptions::default(),
            title: "tcode".into(),
            fonts: vec![Cow::Borrowed(crate::assets::DM_SANS)],
            theme_json: Cow::Borrowed(THEME_JSON),
            activate: false,
            setup: ShellSetup::default(),
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
impl ShellOptions {
    /// Register the bundled monospace family and select it anywhere the shared
    /// theme asks for the desktop-only SF Mono family.
    pub fn with_bundled_monospace(mut self) -> Self {
        self.fonts.extend([
            Cow::Borrowed(crate::assets::LILEX_REGULAR),
            Cow::Borrowed(crate::assets::LILEX_BOLD),
            Cow::Borrowed(crate::assets::LILEX_ITALIC),
            Cow::Borrowed(crate::assets::LILEX_BOLD_ITALIC),
        ]);
        self.theme_json = Cow::Owned(self.theme_json.replace("SF Mono", "Lilex"));
        self
    }
}

/// Open this client's window on the shared shell.
///
/// Returns the window and its shell, so bootstrap can keep doing whatever is
/// genuinely its own (a smoke run, a launch flag, a platform Back callback).
pub fn run_shell(
    cx: &mut App,
    host: Rc<dyn ClientHost>,
    seam: WindowSeam,
    options: ShellOptions,
) -> (WindowHandle<OverlayHost>, Entity<AppShell>) {
    cx.set_global(seam);
    cx.text_system()
        .add_fonts(options.fonts)
        .expect("failed to register bundled application fonts");
    theme::init_with_json(&options.theme_json, cx);
    crate::markdown::init(cx);
    // Global ⌘K / Ctrl-K opens/closes the command palette (handled by
    // AppShell). `secondary` is gpui's platform modifier: command on macOS,
    // control on Windows/Linux — where a literal `cmd-` binding would mean the
    // Super/Win key, which the OS intercepts.
    cx.bind_keys([KeyBinding::new("secondary-k", TogglePalette, None)]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([KeyBinding::new("cmd-q", crate::shell::Quit, None)]);
    if options.activate {
        cx.activate(true);
    }

    let title = options.title;
    let has_local = options.setup.local.is_some();
    let setup = Rc::new(RefCell::new(Some(options.setup)));
    let mounted: Rc<RefCell<Option<Entity<AppShell>>>> = Rc::new(RefCell::new(None));
    // Who this client is, and how it re-points at another host. Installed
    // *before* the window, because the views the window builds — the pair form
    // most of all — ask this global what kind of client they are on.
    // Switching resolves the window's shell when it is actually asked to, not
    // when this global is installed: the shell does not exist yet here, and the
    // cell below is emptied as soon as the window hands it over.
    cx.set_global(ClientAttachment::new(
        host,
        has_local,
        |target, window, cx| {
            crate::shell::switch_current(target, window, cx);
        },
    ));
    let captured = mounted.clone();
    let window = cx
        .open_window(options.window, move |window, cx| {
            window.set_window_title(&title);
            theme::sync_system_appearance(Some(window), cx);
            let window_state = cx.new(|_| WindowState::new(false));
            let shell = cx.new(|cx| {
                AppShell::new(
                    window_state,
                    setup.borrow_mut().take().unwrap_or_default(),
                    window,
                    cx,
                )
            });
            *captured.borrow_mut() = Some(shell.clone());
            cx.new(|cx| OverlayHost::new(shell, window, cx))
        })
        .expect("failed to open the tcode window");
    let shell = mounted
        .borrow_mut()
        .take()
        .expect("the shell is built while the window opens");
    crate::shell::set_back_target(window.into(), &shell, cx);
    let _ = window.update(cx, |_, window, _| {
        if options.activate {
            window.activate_window();
        }
    });
    (window, shell)
}
