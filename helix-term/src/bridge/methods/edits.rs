//! editor/applyEdits (step 2a).
//! Stub: answers `method_not_found` and is absent from `capabilities`.

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::bridge::dispatch::not_implemented;
use crate::bridge::protocol::RpcResult;
use crate::bridge::server::Ctx;

/// Set to `Some("applyEdits")` once implemented.
pub const CAPABILITY: Option<&str> = None;

pub fn apply_edits(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    not_implemented("editor/applyEdits")
}
