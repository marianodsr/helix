//! annotations/set (step 3).
//! Stub: answers `method_not_found` and is absent from `capabilities`.

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::bridge::dispatch::not_implemented;
use crate::bridge::protocol::RpcResult;
use crate::bridge::server::Ctx;

/// Set to `Some("annotations")` once implemented.
pub const CAPABILITY: Option<&str> = None;

pub fn set(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    not_implemented("annotations/set")
}
