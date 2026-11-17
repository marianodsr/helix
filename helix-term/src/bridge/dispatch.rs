//! Method table: name -> handler. Adding a method is one line in `METHODS`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use helix_event::runtime_local;
use helix_view::Editor;
use serde_json::Value;
use tokio::sync::oneshot;

use super::events::{subscribe, unsubscribe};
use super::methods::{
    annotations, command, diagnostics, editor, edits, locations, lsp, reload, reveal, review, views,
};
use super::protocol::{ErrorCode, RpcError, RpcResult};
use super::server::Ctx;
use crate::compositor::Compositor;

pub type Handler = fn(Ctx, Value) -> BoxFuture<'static, RpcResult>;

/// Whether a method can change what is on screen. Helix repaints after
/// every job dispatched to its main loop; after the job of a `ReadOnly`
/// method it does not (see [`skip_render`]), which saves a frame per
/// request (about 7 ms on a 20k-line file). A `ReadOnly` method whose job
/// does change something calls [`repaint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    ReadOnly,
    Changes,
}
use Screen::{Changes, ReadOnly};

const METHODS: &[(&str, Handler, Screen)] = &[
    ("bridge/hello", editor::hello, ReadOnly),
    ("editor/state", editor::state, ReadOnly),
    ("editor/open", editor::open, Changes),
    ("editor/text", editor::text, ReadOnly),
    ("editor/selection", editor::selection, ReadOnly),
    ("editor/status", editor::status, Changes),
    ("events/subscribe", subscribe, ReadOnly),
    ("events/unsubscribe", unsubscribe, ReadOnly),
    // step 2a
    ("editor/reload", reload::reload, Changes),
    ("locations/set", locations::set, Changes),
    ("locations/clear", locations::clear, Changes),
    ("editor/applyEdits", edits::apply_edits, Changes),
    // step 2b
    ("diagnostics/get", diagnostics::get, ReadOnly),
    ("lsp/servers", lsp::servers, ReadOnly),
    ("lsp/query", lsp::query, ReadOnly),
    // step 2c
    ("view/list", views::list, ReadOnly),
    ("view/focus", views::focus, Changes),
    ("view/close", views::close, Changes),
    ("editor/select", views::select, Changes),
    ("editor/command", command::command, Changes),
    // step 2d
    ("annotations/set", annotations::set, Changes),
    ("annotations/clear", annotations::clear, Changes),
    ("editor/reveal", reveal::reveal, Changes),
    // step 5
    ("review/set", review::set, Changes),
    ("review/clear", review::clear, Changes),
    ("review/state", review::state, ReadOnly),
];

/// `bridge/hello` capabilities: each module says whether it is implemented.
pub const CAPABILITIES: &[Option<&str>] = &[
    editor::CAPABILITY,
    reload::CAPABILITY,
    locations::CAPABILITY,
    views::CAPABILITY,
    command::CAPABILITY,
    diagnostics::CAPABILITY,
    lsp::CAPABILITY,
    edits::CAPABILITY,
    annotations::CAPABILITY,
    reveal::CAPABILITY,
    review::CAPABILITY,
];

/// A method from the table.
#[derive(Clone, Copy)]
pub struct Method {
    handler: Handler,
    screen: Screen,
}

impl Method {
    /// Run the handler; every `on_editor` it makes knows its `Screen`.
    pub fn call(self, cx: Ctx, params: Value) -> impl std::future::Future<Output = RpcResult> {
        SCREEN.scope(self.screen, (self.handler)(cx, params))
    }
}

pub fn lookup(method: &str) -> Option<Method> {
    METHODS
        .iter()
        .find(|(name, _, _)| *name == method)
        .map(|&(_, handler, screen)| Method { handler, screen })
}

tokio::task_local! {
    /// The `Screen` of the request this task serves.
    static SCREEN: Screen;
}

runtime_local! {
    /// Set by the job of a read-only request, taken by the main loop.
    static SKIP_RENDER: AtomicBool = AtomicBool::new(false);
    /// Set by a read-only request's job that changed something after all.
    static REPAINT: AtomicBool = AtomicBool::new(false);
}

/// Called by the main loop after each dispatched job: true when that job
/// was a read-only request, so the frame it would draw is the same.
/// Anything else that asked for a redraw still gets one through Helix's
/// own redraw request.
pub fn skip_render() -> bool {
    SKIP_RENDER.swap(false, Ordering::Relaxed)
}

/// From inside a read-only request's job: this one did change the screen
/// (for example a background load that shows in the bufferline).
pub fn repaint() {
    REPAINT.store(true, Ordering::Relaxed);
}

/// Requests that touch the editor answer within this or fail with `timeout`.
pub const EDITOR_TIMEOUT: Duration = Duration::from_secs(2);

/// Run `f` on the main loop with the editor and wait for its result.
/// A request that times out may still run later; its result is dropped.
/// The main loop repaints afterwards unless the request is `ReadOnly`.
pub async fn on_editor<R, F>(f: F) -> Result<R, RpcError>
where
    R: Send + 'static,
    F: FnOnce(&mut Editor, &mut Compositor) -> R + Send + 'static,
{
    let screen = SCREEN.try_with(|s| *s).unwrap_or(Changes);
    run_on_editor(screen, f).await
}

/// `on_editor` for a step that only reads, in a method that may change the
/// screen in another step (the snapshot of `editor/reload`).
pub async fn on_editor_read<R, F>(f: F) -> Result<R, RpcError>
where
    R: Send + 'static,
    F: FnOnce(&mut Editor, &mut Compositor) -> R + Send + 'static,
{
    run_on_editor(ReadOnly, f).await
}

async fn run_on_editor<R, F>(screen: Screen, f: F) -> Result<R, RpcError>
where
    R: Send + 'static,
    F: FnOnce(&mut Editor, &mut Compositor) -> R + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let call = async move {
        crate::job::dispatch(move |editor, compositor| {
            REPAINT.store(false, Ordering::Relaxed);
            let result = f(editor, compositor);
            if screen == ReadOnly && !REPAINT.load(Ordering::Relaxed) {
                SKIP_RENDER.store(true, Ordering::Relaxed);
            }
            let _ = tx.send(result);
        })
        .await;
        rx.await
    };
    match tokio::time::timeout(EDITOR_TIMEOUT, call).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(_)) => Err(RpcError::new(
            ErrorCode::Timeout,
            "the editor dropped the request",
        )),
        Err(_) => Err(RpcError::new(
            ErrorCode::Timeout,
            "the editor did not answer in time",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_has_unique_names() {
        let mut names: Vec<_> = METHODS.iter().map(|(n, _, _)| *n).collect();
        names.sort_unstable();
        let len = names.len();
        names.dedup();
        assert_eq!(names.len(), len);
        assert!(lookup("editor/state").is_some());
        assert!(lookup("editor/nope").is_none());
    }

    #[test]
    fn step_one_capabilities() {
        let caps: Vec<_> = CAPABILITIES.iter().flatten().copied().collect();
        assert!(caps.contains(&"editor"));
    }
}
