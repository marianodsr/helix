//! locations/set, locations/clear, the `:bb-locations` picker and
//! `:bb-next`/`:bb-prev` (step 2a).
//! Stub: answers `method_not_found` and is absent from `capabilities`.

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::bridge::dispatch::not_implemented;
use crate::bridge::protocol::RpcResult;
use crate::bridge::server::Ctx;
use crate::commands::typed::TypableCommand;

/// Set to `Some("locations")` once implemented.
pub const CAPABILITY: Option<&str> = None;

/// `:bb-locations`, `:bb-next`, `:bb-prev`.
pub const COMMANDS: &[TypableCommand] = &[];

pub fn set(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    not_implemented("locations/set")
}

pub fn clear(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    not_implemented("locations/clear")
}
