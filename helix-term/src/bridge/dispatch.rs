//! Method table: name -> handler. Adding a method is one line in `METHODS`.

use std::time::Duration;

use futures_util::future::BoxFuture;
use helix_view::Editor;
use serde_json::Value;
use tokio::sync::oneshot;

use super::events::{subscribe, unsubscribe};
use super::methods::{
    annotations, command, diagnostics, editor, edits, locations, lsp, reload, reveal, views,
};
use super::protocol::{ErrorCode, RpcError, RpcResult};
use super::server::Ctx;
use crate::compositor::Compositor;

pub type Handler = fn(Ctx, Value) -> BoxFuture<'static, RpcResult>;

const METHODS: &[(&str, Handler)] = &[
    ("bridge/hello", editor::hello),
    ("editor/state", editor::state),
    ("editor/open", editor::open),
    ("editor/text", editor::text),
    ("editor/selection", editor::selection),
    ("editor/status", editor::status),
    ("events/subscribe", subscribe),
    ("events/unsubscribe", unsubscribe),
    // step 2a
    ("editor/reload", reload::reload),
    ("locations/set", locations::set),
    ("locations/clear", locations::clear),
    ("editor/applyEdits", edits::apply_edits),
    // step 2b
    ("diagnostics/get", diagnostics::get),
    ("lsp/servers", lsp::servers),
    ("lsp/query", lsp::query),
    // step 2c
    ("view/list", views::list),
    ("view/focus", views::focus),
    ("view/close", views::close),
    ("editor/select", views::select),
    ("editor/command", command::command),
    // step 2d
    ("annotations/set", annotations::set),
    ("annotations/clear", annotations::clear),
    ("editor/reveal", reveal::reveal),
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
];

pub fn lookup(method: &str) -> Option<Handler> {
    METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, handler)| *handler)
}

/// Requests that touch the editor answer within this or fail with `timeout`.
pub const EDITOR_TIMEOUT: Duration = Duration::from_secs(2);

/// Run `f` on the main loop with the editor and wait for its result.
/// A request that times out may still run later; its result is dropped.
pub async fn on_editor<R, F>(f: F) -> Result<R, RpcError>
where
    R: Send + 'static,
    F: FnOnce(&mut Editor, &mut Compositor) -> R + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let call = async move {
        crate::job::dispatch(move |editor, compositor| {
            let _ = tx.send(f(editor, compositor));
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
        let mut names: Vec<_> = METHODS.iter().map(|(n, _)| *n).collect();
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
