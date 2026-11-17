//! view/list, view/focus, view/close, editor/select (step 2c).
//! Stub: answers `method_not_found` and is absent from `capabilities`.

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::bridge::dispatch::not_implemented;
use crate::bridge::protocol::RpcResult;
use crate::bridge::server::Ctx;

/// Set to `Some("views")` once implemented.
pub const CAPABILITY: Option<&str> = None;

pub fn list(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    not_implemented("view/list")
}

pub fn focus(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    not_implemented("view/focus")
}

pub fn close(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    not_implemented("view/close")
}

pub fn select(_cx: Ctx, _params: Value) -> BoxFuture<'static, RpcResult> {
    not_implemented("editor/select")
}
