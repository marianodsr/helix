//! Helix watches the files of its open buffers (capability `watch`).
//!
//! How (macOS): kqueue, through `libc`, on one thread per bridge that blocks
//! in `kevent()` and wakes only when a watched file or directory changes, or
//! when the set of open buffers changes (an `EVFILT_USER` trigger). For each
//! open buffer's file it watches
//!
//! - the file itself (`NOTE_WRITE`, `NOTE_EXTEND`: written in place;
//!   `NOTE_DELETE`, `NOTE_RENAME`, `NOTE_REVOKE`: gone from under the
//!   watch), and
//! - its parent directory, not recursively (`NOTE_WRITE`: an entry was
//!   created, removed or renamed), so a file replaced by renaming a temporary
//!   file over it, or deleted and created again, is noticed and watched
//!   again. When the path goes through a symlink, the directory of the real
//!   file is watched too.
//!
//! A directory event only stats the watched files in that directory and
//! reports the ones whose identity (device, inode) changed, appeared or
//! disappeared. Changed paths go to a task that coalesces a burst (30 ms)
//! and hands them to the same reload as `editor/reload` (`reload::watched`).
//!
//! Why not FSEvents (the `notify` crate's macOS default): it watches
//! directories recursively, so a build in the repository would wake the
//! editor, and it batches deliveries. kqueue costs a descriptor per file and
//! per directory: the soft `RLIMIT_NOFILE` is raised (up to 10240) when the
//! watch starts, and at most half of it is used; files beyond that are not
//! watched (logged once) and still caught by the focus check.
//!
//! Other platforms: not watched yet (no `watch` capability).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use helix_event::{register_hook, runtime_local};
use helix_view::events::{DocumentDidClose, DocumentDidOpen};
use helix_view::Editor;
use parking_lot::Mutex;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

use crate::bridge::dispatch::on_editor_read;
use crate::bridge::Shared;

pub const CAPABILITY: Option<&str> = if cfg!(target_os = "macos") {
    Some("watch")
} else {
    None
};

/// Events closer together than this are one change.
const COALESCE: Duration = Duration::from_millis(30);
/// How long a change waits for a save Helix has in flight to be recorded.
const SAVE_WAIT: Duration = Duration::from_secs(2);

runtime_local! {
    /// Is the watch running (started, and `[editor.bridge] watch` on)?
    static RUNNING: AtomicBool = AtomicBool::new(false);
    static HANDLE: Mutex<Option<imp::Handle>> = Mutex::new(None);
}

/// Files watched, and files that could not be (descriptor budget). Shared
/// with the watch thread, which has no runtime for `runtime_local!`.
#[derive(Default)]
pub struct Counts {
    watched: AtomicUsize,
    unwatched: AtomicUsize,
}

/// Whether Helix is watching its open files (for `bridge/hello`).
pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

/// Files watched and files over the descriptor budget (`:bb-status`, tests).
pub fn counts() -> (usize, usize) {
    match HANDLE.lock().as_ref() {
        Some(handle) => (
            handle.counts.watched.load(Ordering::Relaxed),
            handle.counts.unwatched.load(Ordering::Relaxed),
        ),
        None => (0, 0),
    }
}

/// Start watching, if this platform can and the configuration wants it.
/// Called once from `bridge::start`.
pub fn start(editor: &Editor, shared: &std::sync::Arc<Shared>) {
    if CAPABILITY.is_none() || !editor.config().bridge.watch {
        return;
    }
    let (tx, rx) = unbounded_channel();
    let Some(handle) = imp::Handle::spawn(tx) else {
        return;
    };
    *HANDLE.lock() = Some(handle);
    RUNNING.store(true, Ordering::Relaxed);
    let task = tokio::spawn(run(rx));
    shared.track(task.abort_handle());
    resync(editor);
}

/// Stop the watch thread (bridge shutdown).
pub fn stop() {
    RUNNING.store(false, Ordering::Relaxed);
    if let Some(handle) = HANDLE.lock().take() {
        handle.stop();
    }
}

/// Watch exactly the files of the open buffers. Main thread: only collects
/// the paths; the watch thread opens and closes descriptors.
pub fn resync(editor: &Editor) {
    if !running() {
        return;
    }
    let paths: Vec<PathBuf> = editor
        .documents()
        .filter_map(|doc| doc.path().map(|p| p.to_path_buf()))
        .collect();
    if let Some(handle) = HANDLE.lock().as_ref() {
        handle.set(paths);
    }
}

/// After a save: `:write <path>` gives the buffer its new path only after
/// the save hook, so look once the main loop is free again.
pub fn resync_after_save() {
    if running() {
        tokio::spawn(async {
            let _ = on_editor_read(|editor, _| resync(editor)).await;
        });
    }
}

/// Hooks: a buffer opened or closed changes what is watched. (A buffer's
/// path changes by `:write <path>`, which the save hook covers, or `:move`,
/// which the rename in the old directory reports.)
pub fn register_hooks() {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        resync(event.editor);
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        resync(event.editor);
        Ok(())
    });
}

/// Coalesce what the watch thread reports, then reload. One batch at a
/// time: changes that arrive during a reload are the next batch, so a write
/// that lands while a reload is in flight is reloaded after it.
async fn run(mut rx: UnboundedReceiver<PathBuf>) {
    while let Some(first) = rx.recv().await {
        let mut paths = vec![first];
        let deadline = tokio::time::Instant::now() + COALESCE;
        loop {
            tokio::select! {
                path = rx.recv() => match path {
                    Some(path) => {
                        if !paths.contains(&path) {
                            paths.push(path);
                        }
                    }
                    None => return,
                },
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        wait_for_saves().await;
        super::reload::watched(paths).await;
        // `:move` renamed a buffer's file: watch the new path
        let _ = on_editor_read(|editor, _| resync(editor)).await;
    }
}

/// Helix's own save is reported by the watch before Helix has recorded it
/// (the saved revision and the file's new mtime). Wait for that, so the save
/// is not taken for someone else's change.
async fn wait_for_saves() {
    let until = tokio::time::Instant::now() + SAVE_WAIT;
    while tokio::time::Instant::now() < until {
        match on_editor_read(|editor, _| editor.write_count).await {
            Ok(0) | Err(_) => return,
            Ok(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;

    use tokio::sync::mpsc::UnboundedSender;

    use std::sync::Arc;

    use super::Counts;

    const FILE_EVENTS: u32 = libc::NOTE_WRITE
        | libc::NOTE_EXTEND
        | libc::NOTE_DELETE
        | libc::NOTE_RENAME
        | libc::NOTE_REVOKE;
    const DIR_EVENTS: u32 =
        libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;
    const GONE: u32 = libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;
    /// Highest soft descriptor limit the watch asks for.
    const WANT_FILES: libc::rlim_t = 10240;

    enum Ctl {
        Set(Vec<PathBuf>),
        Stop,
    }

    pub struct Handle {
        kq: libc::c_int,
        ctl: mpsc::Sender<Ctl>,
        pub counts: Arc<Counts>,
    }

    impl Handle {
        pub fn spawn(tx: UnboundedSender<PathBuf>) -> Option<Handle> {
            // SAFETY: kqueue has no preconditions.
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                log::error!("bridge: watch: kqueue: {}", std::io::Error::last_os_error());
                return None;
            }
            // the wake-up for control messages
            register(kq, 0, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0);
            let budget = descriptor_budget();
            let (ctl, rx) = mpsc::channel();
            let counts = Arc::new(Counts::default());
            let shared = counts.clone();
            let spawned = std::thread::Builder::new()
                .name("bb-watch".into())
                .spawn(move || Watcher::new(kq, budget, tx, shared).run(rx));
            if let Err(err) = spawned {
                log::error!("bridge: watch: {err}");
                // SAFETY: kq is ours.
                unsafe { libc::close(kq) };
                return None;
            }
            Some(Handle { kq, ctl, counts })
        }

        pub fn set(&self, paths: Vec<PathBuf>) {
            if self.ctl.send(Ctl::Set(paths)).is_ok() {
                self.wake();
            }
        }

        pub fn stop(&self) {
            if self.ctl.send(Ctl::Stop).is_ok() {
                self.wake();
            }
        }

        fn wake(&self) {
            register(self.kq, 0, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
        }
    }

    fn register(kq: libc::c_int, ident: usize, filter: i16, flags: u16, fflags: u32) -> bool {
        let change = libc::kevent {
            ident,
            filter,
            flags,
            fflags,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one valid change, no event buffer.
        let n = unsafe { libc::kevent(kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        n >= 0
    }

    /// Raise the soft descriptor limit (macOS GUI sessions start at 256)
    /// and keep half of it for the watch.
    fn descriptor_budget() -> usize {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: lim is a valid out pointer.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
            return 64;
        }
        let want = WANT_FILES.min(lim.rlim_max);
        if lim.rlim_cur < want {
            let raised = libc::rlimit {
                rlim_cur: want,
                rlim_max: lim.rlim_max,
            };
            // SAFETY: a valid limit no higher than the hard one.
            if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
                lim.rlim_cur = want;
            }
        }
        (lim.rlim_cur.min(WANT_FILES) / 2) as usize
    }

    fn open_evtonly(path: &Path) -> Option<libc::c_int> {
        let c = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: c is a valid C string.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        (fd >= 0).then_some(fd)
    }

    /// (device, inode) of what the path names now, following symlinks.
    fn identity(path: &Path) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }

    enum Target {
        File(PathBuf),
        Dir(PathBuf),
    }

    struct File {
        fd: Option<libc::c_int>,
        id: Option<(u64, u64)>,
        dirs: Vec<PathBuf>,
    }

    struct Dir {
        fd: Option<libc::c_int>,
        files: Vec<PathBuf>,
    }

    struct Watcher {
        kq: libc::c_int,
        budget: usize,
        used: usize,
        warned: bool,
        files: HashMap<PathBuf, File>,
        dirs: HashMap<PathBuf, Dir>,
        fds: HashMap<libc::c_int, Target>,
        tx: UnboundedSender<PathBuf>,
        counts: Arc<Counts>,
    }

    impl Watcher {
        fn new(
            kq: libc::c_int,
            budget: usize,
            tx: UnboundedSender<PathBuf>,
            counts: Arc<Counts>,
        ) -> Watcher {
            Watcher {
                kq,
                budget,
                used: 0,
                warned: false,
                files: HashMap::new(),
                dirs: HashMap::new(),
                fds: HashMap::new(),
                tx,
                counts,
            }
        }

        fn run(mut self, ctl: mpsc::Receiver<Ctl>) {
            let empty = libc::kevent {
                ident: 0,
                filter: 0,
                flags: 0,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let mut events = [empty; 64];
            loop {
                // SAFETY: the buffer is valid for its length; no timeout,
                // so the thread sleeps until something happens.
                let n = unsafe {
                    libc::kevent(
                        self.kq,
                        std::ptr::null(),
                        0,
                        events.as_mut_ptr(),
                        events.len() as libc::c_int,
                        std::ptr::null(),
                    )
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    log::error!("bridge: watch: kevent: {err}");
                    break;
                }
                let mut changed = Vec::new();
                for ev in &events[..n as usize] {
                    if ev.filter == libc::EVFILT_USER {
                        while let Ok(msg) = ctl.try_recv() {
                            match msg {
                                Ctl::Set(paths) => self.set(paths),
                                Ctl::Stop => {
                                    self.close_all();
                                    return;
                                }
                            }
                        }
                    } else {
                        self.event(ev.ident as libc::c_int, ev.fflags, &mut changed);
                    }
                }
                for path in changed {
                    if self.tx.send(path).is_err() {
                        self.close_all();
                        return;
                    }
                }
                self.publish();
            }
            self.close_all();
        }

        fn publish(&self) {
            self.counts.watched.store(
                self.files.values().filter(|f| f.fd.is_some()).count(),
                Ordering::Relaxed,
            );
            let over = self
                .files
                .values()
                .filter(|f| f.fd.is_none() && f.id.is_some())
                .count();
            self.counts.unwatched.store(over, Ordering::Relaxed);
        }

        fn set(&mut self, paths: Vec<PathBuf>) {
            let gone: Vec<PathBuf> = self
                .files
                .keys()
                .filter(|p| !paths.contains(p))
                .cloned()
                .collect();
            for path in gone {
                self.remove(&path);
            }
            for path in paths {
                match self.files.get(&path) {
                    None => self.add(path),
                    // retry what is missing: a directory that came back, a
                    // file over the budget earlier
                    Some(file) => {
                        let unwatched = file.fd.is_none();
                        let missing: Vec<PathBuf> = file
                            .dirs
                            .iter()
                            .filter(|d| self.dirs.get(*d).is_none_or(|d| d.fd.is_none()))
                            .cloned()
                            .collect();
                        for dir in missing {
                            self.open_dir(&dir);
                        }
                        if unwatched && identity(&path).is_some() {
                            self.open_file(&path);
                        }
                    }
                }
            }
            self.publish();
        }

        fn add(&mut self, path: PathBuf) {
            let mut dirs = Vec::new();
            if let Some(parent) = path.parent() {
                dirs.push(parent.to_path_buf());
            }
            if let Some(real) = std::fs::canonicalize(&path)
                .ok()
                .and_then(|r| r.parent().map(Path::to_path_buf))
            {
                if !dirs.contains(&real) {
                    dirs.push(real);
                }
            }
            for dir in &dirs {
                let entry = self.dirs.entry(dir.clone()).or_insert(Dir {
                    fd: None,
                    files: Vec::new(),
                });
                entry.files.push(path.clone());
                if entry.fd.is_none() {
                    self.open_dir(dir);
                }
            }
            self.files.insert(
                path.clone(),
                File {
                    fd: None,
                    id: None,
                    dirs,
                },
            );
            self.open_file(&path);
        }

        fn remove(&mut self, path: &Path) {
            let Some(file) = self.files.remove(path) else {
                return;
            };
            if let Some(fd) = file.fd {
                self.close(fd);
            }
            for dir in file.dirs {
                let empty = match self.dirs.get_mut(&dir) {
                    Some(d) => {
                        d.files.retain(|p| p != path);
                        d.files.is_empty()
                    }
                    None => false,
                };
                if empty {
                    if let Some(fd) = self.dirs.remove(&dir).and_then(|d| d.fd) {
                        self.close(fd);
                    }
                }
            }
        }

        fn take_slot(&mut self) -> bool {
            if self.used < self.budget {
                self.used += 1;
                return true;
            }
            if !self.warned {
                self.warned = true;
                log::warn!(
                    "bridge: watch: {} descriptors in use, not watching more files \
                     (the focus check still catches their changes)",
                    self.used
                );
            }
            false
        }

        fn watch_fd(&mut self, path: &Path, events: u32) -> Option<libc::c_int> {
            if !self.take_slot() {
                return None;
            }
            let Some(fd) = open_evtonly(path) else {
                self.used -= 1;
                return None;
            };
            if !register(
                self.kq,
                fd as usize,
                libc::EVFILT_VNODE,
                libc::EV_ADD | libc::EV_CLEAR,
                events,
            ) {
                // SAFETY: fd is ours.
                unsafe { libc::close(fd) };
                self.used -= 1;
                return None;
            }
            Some(fd)
        }

        fn open_dir(&mut self, dir: &Path) {
            let fd = self.watch_fd(dir, DIR_EVENTS);
            if let Some(fd) = fd {
                self.fds.insert(fd, Target::Dir(dir.to_path_buf()));
            }
            if let Some(d) = self.dirs.get_mut(dir) {
                d.fd = fd;
            }
        }

        /// (Re)open the watch on the file the path names now.
        fn open_file(&mut self, path: &Path) {
            let old = self.files.get_mut(path).and_then(|f| f.fd.take());
            if let Some(fd) = old {
                self.close(fd);
            }
            let id = identity(path);
            let fd = match id {
                Some(_) => self.watch_fd(path, FILE_EVENTS),
                None => None,
            };
            if let Some(fd) = fd {
                self.fds.insert(fd, Target::File(path.to_path_buf()));
            }
            if let Some(file) = self.files.get_mut(path) {
                file.fd = fd;
                file.id = id;
            }
        }

        fn close(&mut self, fd: libc::c_int) {
            self.fds.remove(&fd);
            self.used = self.used.saturating_sub(1);
            // SAFETY: fd is ours; closing it also removes its kevent.
            unsafe { libc::close(fd) };
        }

        fn close_all(&mut self) {
            let fds: Vec<_> = self.fds.keys().copied().collect();
            for fd in fds {
                self.close(fd);
            }
            // SAFETY: the kqueue is ours and nothing waits on it any more.
            unsafe { libc::close(self.kq) };
            self.counts.watched.store(0, Ordering::Relaxed);
        }

        fn event(&mut self, fd: libc::c_int, fflags: u32, changed: &mut Vec<PathBuf>) {
            match self.fds.get(&fd) {
                Some(Target::File(path)) => {
                    let path = path.clone();
                    if fflags & GONE != 0 {
                        // replaced or deleted: watch whatever is there now
                        self.open_file(&path);
                    }
                    push(changed, path);
                }
                Some(Target::Dir(dir)) => {
                    let dir = dir.clone();
                    if fflags & GONE != 0 {
                        // the directory itself moved or went away
                        if let Some(old) = self.dirs.get_mut(&dir).and_then(|d| d.fd.take()) {
                            self.close(old);
                        }
                        self.open_dir(&dir);
                    }
                    let files = self
                        .dirs
                        .get(&dir)
                        .map(|d| d.files.clone())
                        .unwrap_or_default();
                    for path in files {
                        let now = identity(&path);
                        let before = self.files.get(&path).and_then(|f| f.id);
                        let unwatched = self.files.get(&path).is_some_and(|f| f.fd.is_none());
                        if now != before || (unwatched && now.is_some()) {
                            self.open_file(&path);
                            push(changed, path);
                        }
                    }
                }
                None => {}
            }
        }
    }

    fn push(changed: &mut Vec<PathBuf>, path: PathBuf) {
        if !changed.contains(&path) {
            changed.push(path);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::PathBuf;
    use tokio::sync::mpsc::UnboundedSender;

    pub struct Handle;

    impl Handle {
        pub fn spawn(_tx: UnboundedSender<PathBuf>) -> Option<Handle> {
            None
        }
        pub fn set(&self, _paths: Vec<PathBuf>) {}
        pub fn stop(&self) {}
    }
}
