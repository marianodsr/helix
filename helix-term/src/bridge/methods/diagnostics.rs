//! diagnostics/get and `event/diagnostics` (step 2b).
//! Stub: answers `method_not_found` and is absent from `capabilities`.
//! The `diagnostics` event kind is accepted by `events/subscribe` but emits
//! nothing yet.

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::bridge::dispatch::not_implemented;
use crate::bridge::protocol::RpcResult;
use crate::bridge::server::Ctx;

/// Set to `Some("diagnostics")` once implemented.
pub const CAPABILITY: Option<&str> = None;

/// Hooks this module needs (registered when the bridge starts).
pub fn register_hooks() {}

pub fn get(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    not_implemented("diagnostics/get")
}
