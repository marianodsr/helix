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
    fn track(&self, handle: AbortHandle) {
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
    if !editor.config().bridge.enable || std::env::var_os("HELIX_BRIDGE").is_some_and(|v| v == "0")
    {
        return;
    }
    let Some(dir) = bridge_dir() else {
        return;
    };
    let pid = std::process::id();
    let info = Info {
        pid,
        socket: dir.join(format!("{pid}.sock")),
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
    let task = tokio::spawn(server::listen(dir, shared.clone()));
    shared.track(task.abort_handle());
    log::info!("bridge: started in {:?}", begin.elapsed());
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

/// The socket of this runtime's bridge, if it started.
pub fn socket_path() -> Option<PathBuf> {
    BRIDGE.get().map(|s| s.info.socket.clone())
}
