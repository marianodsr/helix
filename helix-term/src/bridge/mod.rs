//! bb bridge: a local socket that lets bb (and its agents) talk to this Helix.
//!
//! Design: bb-bridge/ARCHITECTURE.md. Wire contract: bb-bridge/PROTOCOL.md.
//! The server is a tokio task; anything that needs the editor goes through
//! `job::dispatch`. Hooks only forward small snapshots to the event task.

mod convert;
mod dispatch;
mod events;
mod methods;
mod protocol;
mod server;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use helix_event::runtime_local;
use helix_view::{DocumentId, Editor};
use parking_lot::Mutex;
use tokio::task::AbortHandle;

pub use methods::annotations::decorate;
pub use methods::files::{hand_over_terminal, pausable_input};
pub use methods::typable_commands;

runtime_local! {
    static BRIDGE: OnceLock<Arc<Shared>> = OnceLock::new();
}

#[cfg(feature = "integration")]
runtime_local! {
    static TEST_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
}

/// Facts about this instance, fixed at start.
pub(crate) struct Info {
    pub pid: u32,
    pub socket: PathBuf,
    pub registry: PathBuf,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    pub started_at: SystemTime,
}

/// State shared by the listener, the clients and the event task.
pub(crate) struct Shared {
    pub info: Info,
    pub clients: Mutex<Vec<Arc<server::Client>>>,
    subs: Mutex<[usize; events::Kind::COUNT]>,
    tasks: Mutex<Vec<AbortHandle>>,
    stopped: std::sync::atomic::AtomicBool,
}

impl Shared {
    pub(crate) fn track(&self, handle: AbortHandle) {
        self.tasks.lock().push(handle);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Apply a change to one client's subscriptions and refresh the global
    /// "is anyone listening" flags the hooks read.
    pub fn update_subscriptions(&self, before: u32, after: u32) {
        let mut subs = self.subs.lock();
        for kind in events::Kind::ALL {
            let bit = kind.bit();
            match (before & bit != 0, after & bit != 0) {
                (false, true) => subs[kind as usize] += 1,
                (true, false) => subs[kind as usize] -= 1,
                _ => {}
            }
        }
        let wants = events::Kind::ALL
            .iter()
            .filter(|k| subs[**k as usize] > 0)
            .fold(0, |acc, k| acc | k.bit());
        events::set_wants(wants);
    }

    fn shutdown(&self) {
        if self.stopped.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        events::set_wants(0);
        methods::watch::stop();
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
        for client in self.clients.lock().drain(..) {
            client.close();
        }
        let _ = std::fs::remove_file(&self.info.registry);
        let _ = std::fs::remove_file(&self.info.socket);
    }
}

/// Start the bridge if it is enabled. Called once from `Application::new`
/// after the editor exists; binding the socket happens in a background task.
pub fn start(editor: &Editor) {
    let begin = Instant::now();
    if !enabled(
        editor.config().bridge.enable,
        std::env::var_os("HELIX_BRIDGE"),
    ) {
        return;
    }
    let Some(dir) = bridge_dir() else {
        return;
    };
    let pid = std::process::id();
    let info = Info {
        pid,
        socket: socket_path_for(&dir, pid),
        registry: dir.join(format!("{pid}.json")),
        cwd: helix_stdx::env::current_working_dir(),
        workspace_root: helix_loader::find_workspace().0,
        started_at: SystemTime::now(),
    };
    let shared = Arc::new(Shared {
        info,
        clients: Mutex::new(Vec::new()),
        subs: Mutex::new([0; events::Kind::COUNT]),
        tasks: Mutex::new(Vec::new()),
        stopped: false.into(),
    });
    if BRIDGE.set(shared.clone()).is_err() {
        return;
    }
    events::install(editor, &shared);
    methods::command::capture_policy(editor);
    methods::watch::start(editor, &shared);
    let task = tokio::spawn(server::listen(dir, shared.clone()));
    shared.track(task.abort_handle());
    log::info!("bridge: started in {:?}", begin.elapsed());
}

/// Socket paths are limited to 104 bytes on macOS (108 on Linux).
const MAX_SOCKET_PATH: usize = 100;

/// `<dir>/<pid>.sock`, or `/tmp/helix-bridge-<uid>/<pid>.sock` when that
/// would be too long. The registry file always names the one in use.
fn socket_path_for(dir: &std::path::Path, pid: u32) -> PathBuf {
    let socket = dir.join(format!("{pid}.sock"));
    if socket.as_os_str().len() <= MAX_SOCKET_PATH {
        return socket;
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/helix-bridge-{uid}/{pid}.sock"))
}

/// `[editor.bridge] enable`, overridden off by `HELIX_BRIDGE=0`.
fn enabled(config: bool, env: Option<std::ffi::OsString>) -> bool {
    config && env.is_none_or(|v| v != "0")
}

/// `HELIX_KITTY_KEYBOARD=enabled|disabled` overrides `kitty-keyboard-protocol`
/// at start. Inside bb, bb's own terminal answers the capability query
/// before the editor surface can, so `auto` never turns the protocol on
/// there; the plugin sets this on the sessions it starts. Called from
/// `main` before the terminal is set up.
pub fn kitty_keyboard_from_env(mut config: crate::config::Config) -> crate::config::Config {
    if let Some(value) = std::env::var_os("HELIX_KITTY_KEYBOARD") {
        match kitty_keyboard(&value) {
            Some(setting) => config.editor.kitty_keyboard_protocol = setting,
            None => log::warn!("HELIX_KITTY_KEYBOARD={value:?} ignored: use enabled or disabled"),
        }
    }
    config
}

fn kitty_keyboard(
    value: &std::ffi::OsStr,
) -> Option<helix_view::editor::KittyKeyboardProtocolConfig> {
    use helix_view::editor::KittyKeyboardProtocolConfig as Kitty;
    match value.to_str()? {
        "enabled" => Some(Kitty::Enabled),
        "disabled" => Some(Kitty::Disabled),
        _ => None,
    }
}

/// Remove the socket and registry file and drop every client. Called from
/// `Application::close`.
pub fn stop() {
    if let Some(shared) = BRIDGE.get() {
        shared.shutdown();
    }
}

/// A document was written to disk (there is no editor event for this).
pub fn document_saved(editor: &Editor, doc: DocumentId) {
    events::document_saved(editor, doc);
    // `:write <path>` gives the buffer a new file
    methods::watch::resync_after_save();
}

/// The terminal regained focus: reload buffers changed on disk meanwhile.
pub fn focus_gained(editor: &mut Editor) {
    methods::reload::focus_gained(editor);
}

/// After a dispatched job: was it a read-only bridge request, so there is
/// nothing new to draw? Called by the main loop.
pub fn skip_render() -> bool {
    dispatch::skip_render()
}

/// A language server published diagnostics for `uri`, open or not.
pub fn diagnostics_published(uri: &helix_core::Uri) {
    methods::diagnostics::published(uri);
}

pub(crate) fn shared() -> Option<&'static Arc<Shared>> {
    BRIDGE.get().filter(|s| !s.is_stopped())
}

#[cfg(not(feature = "integration"))]
fn bridge_dir() -> Option<PathBuf> {
    Some(helix_loader::cache_dir().join("bridge"))
}

/// Integration tests run many editors in one process, so the bridge only
/// starts in tests that ask for it, in a directory of their own.
#[cfg(feature = "integration")]
fn bridge_dir() -> Option<PathBuf> {
    TEST_DIR.lock().clone()
}

/// Test support: make the next `Application` in this runtime start a bridge
/// in `dir`.
#[cfg(feature = "integration")]
pub fn set_test_dir(dir: PathBuf) {
    *TEST_DIR.lock() = Some(dir);
}

#[cfg(feature = "integration")]
runtime_local! {
    static VIEWS_DRAWN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
}

#[cfg(feature = "integration")]
fn count_view_drawn() {
    VIEWS_DRAWN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Test support: how many times a pane has been drawn in this runtime, to
/// tell which requests repaint.
#[cfg(feature = "integration")]
pub fn views_drawn() -> u64 {
    VIEWS_DRAWN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Test support: what is open on top of the editor, bottom to top: `":"`
/// (the command line), `"prompt"` (any other prompt), `"picker"`,
/// `"popup"`, or a component name.
#[cfg(feature = "integration")]
pub async fn overlays() -> Vec<String> {
    use methods::command::{layers, Layer};
    dispatch::on_editor(|_, compositor| {
        layers(compositor, |layers| {
            layers
                .iter()
                .rev()
                .map(|layer| match layer {
                    Layer::Prompt { command: true, .. } => ":".to_owned(),
                    Layer::Prompt { .. } => "prompt".to_owned(),
                    Layer::Picker { .. } => "picker".to_owned(),
                    Layer::Popup => "popup".to_owned(),
                    Layer::Other(name) => (*name).to_owned(),
                })
                .collect()
        })
    })
    .await
    .unwrap_or_default()
}

/// Test support: files the watch has a descriptor on, and files over the
/// descriptor budget.
#[cfg(feature = "integration")]
pub fn watch_counts() -> (usize, usize) {
    methods::watch::counts()
}

/// Test support: the status line, read on the main loop.
#[cfg(feature = "integration")]
pub async fn status() -> Option<String> {
    dispatch::on_editor(|editor, _| editor.get_status().map(|(m, _)| m.to_string()))
        .await
        .ok()
        .flatten()
}

/// The socket of this runtime's bridge, if it started.
pub fn socket_path() -> Option<PathBuf> {
    BRIDGE.get().map(|s| s.info.socket.clone())
}

#[cfg(test)]
mod tests {
    use super::{enabled, socket_path_for};
    use std::path::Path;

    #[test]
    fn long_socket_paths_fall_back_to_tmp() {
        let short = socket_path_for(Path::new("/home/me/.cache/helix/bridge"), 42);
        assert_eq!(short, Path::new("/home/me/.cache/helix/bridge/42.sock"));
        let deep = format!(
            "/home/me/{}/helix/bridge",
            "very-long-directory-name/".repeat(4)
        );
        let long = socket_path_for(Path::new(&deep), 42);
        // SAFETY: no preconditions.
        let uid = unsafe { libc::getuid() };
        assert_eq!(long, Path::new(&format!("/tmp/helix-bridge-{uid}/42.sock")));
    }
    use helix_view::editor::{BridgeCommandPolicy, Config};

    #[test]
    fn kitty_keyboard_values() {
        use helix_view::editor::KittyKeyboardProtocolConfig as Kitty;
        assert_eq!(
            super::kitty_keyboard("enabled".as_ref()),
            Some(Kitty::Enabled)
        );
        assert_eq!(
            super::kitty_keyboard("disabled".as_ref()),
            Some(Kitty::Disabled)
        );
        assert_eq!(super::kitty_keyboard("auto".as_ref()), None);
        assert_eq!(super::kitty_keyboard("".as_ref()), None);
    }

    #[test]
    fn env_switch() {
        assert!(enabled(true, None));
        assert!(enabled(true, Some("1".into())));
        assert!(!enabled(true, Some("0".into())));
        assert!(!enabled(false, None));
        assert!(!enabled(false, Some("1".into())));
    }

    #[test]
    fn config_section() {
        let config = Config::default();
        assert!(config.bridge.enable);
        assert_eq!(config.bridge.commands, BridgeCommandPolicy::Safe);

        let config: Config =
            toml::from_str("[bridge]\nenable = false\ncommands = \"all\"\n").unwrap();
        assert!(!config.bridge.enable);
        assert_eq!(config.bridge.commands, BridgeCommandPolicy::All);
        let config: Config = toml::from_str("[bridge]\ncommands = \"none\"\n").unwrap();
        assert!(config.bridge.enable);
        assert_eq!(config.bridge.commands, BridgeCommandPolicy::None);

        assert!(toml::from_str::<Config>("[bridge]\ncommands = \"some\"\n").is_err());
        assert!(toml::from_str::<Config>("[bridge]\nport = 3\n").is_err());
    }
}
