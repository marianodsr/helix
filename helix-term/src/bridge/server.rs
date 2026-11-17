//! Listener, per-client reader and writer, framing, cancellation and the
//! bounded outbound queues.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Notify, Semaphore};

use super::protocol::{
    self, ErrorCode, OverflowEvent, RpcError, RpcResult, MAX_LINE, PROTOCOL_VERSION, QUEUE_LEN,
};
use super::{convert, dispatch, Shared};

/// Requests one client may have in flight before the reader stops reading.
const MAX_IN_FLIGHT: usize = 64;

pub type Line = Arc<[u8]>;

/// One JSON message as a newline-terminated line.
pub fn encode(value: &Value) -> Line {
    let mut buf = serde_json::to_vec(value).unwrap_or_default();
    buf.push(b'\n');
    buf.into()
}

// --- listener ----------------------------------------------------------------

pub async fn listen(dir: PathBuf, shared: Arc<Shared>) {
    let listener = match bind(&dir, &shared.info.socket) {
        Ok(listener) => listener,
        Err(err) => {
            log::error!("bridge: cannot listen on {:?}: {err}", shared.info.socket);
            return;
        }
    };
    if let Err(err) = write_registry(&shared) {
        log::error!("bridge: cannot write {:?}: {err}", shared.info.registry);
    }
    if shared.is_stopped() {
        // `stop` ran while we were binding.
        let _ = std::fs::remove_file(&shared.info.registry);
        let _ = std::fs::remove_file(&shared.info.socket);
        return;
    }
    log::info!("bridge: listening on {:?}", shared.info.socket);
    let mut next_id = 0;
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(err) => {
                log::warn!("bridge: accept failed: {err}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        next_id += 1;
        let client = Arc::new(Client::new(next_id));
        shared.clients.lock().push(client.clone());
        tokio::spawn(serve(stream, client, shared.clone()));
    }
}

fn bind(dir: &Path, socket: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    // The pid is ours, so anything already there was left by a process that
    // died without cleaning up.
    match std::fs::remove_file(socket) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn write_registry(shared: &Shared) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let info = &shared.info;
    let env: serde_json::Map<String, Value> = [
        "BB_THREAD_ID",
        "BB_ENVIRONMENT_ID",
        "BB_PROJECT_ID",
        "BB_TERMINAL_ID",
    ]
    .into_iter()
    .filter_map(|name| Some((name.to_owned(), Value::String(std::env::var(name).ok()?))))
    .collect();
    let registry = serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "pid": info.pid,
        "socket": info.socket.to_string_lossy(),
        "helixVersion": helix_loader::VERSION_AND_GIT_HASH,
        "startedAt": convert::rfc3339(info.started_at),
        "cwd": info.cwd.to_string_lossy(),
        "workspaceRoot": info.workspace_root.to_string_lossy(),
        "env": env,
    });
    let tmp = info.registry.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(&registry)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&tmp, &info.registry)
}

// --- clients -----------------------------------------------------------------

pub struct Client {
    pub id: u64,
    /// `client` and `clientVersion` from `bridge/hello`.
    pub name: Mutex<Option<String>>,
    subs: AtomicU32,
    outbox: Mutex<Outbox>,
    wake: Notify,
    closed: AtomicBool,
    closing: Notify,
    in_flight: Mutex<HashMap<String, oneshot::Sender<()>>>,
    pub dropped_total: AtomicU64,
}

#[derive(Default)]
struct Outbox {
    items: VecDeque<(Line, bool)>,
    /// Events dropped since the last `event/overflow`.
    dropped: u64,
}

impl Client {
    fn new(id: u64) -> Client {
        Client {
            id,
            name: Mutex::new(None),
            subs: AtomicU32::new(0),
            outbox: Mutex::new(Outbox::default()),
            wake: Notify::new(),
            closed: AtomicBool::new(false),
            closing: Notify::new(),
            in_flight: Mutex::new(HashMap::new()),
            dropped_total: AtomicU64::new(0),
        }
    }

    pub fn subscriptions(&self) -> u32 {
        self.subs.load(Ordering::Relaxed)
    }

    /// Change this client's subscriptions; returns the new mask.
    pub fn set_subscriptions(&self, shared: &Shared, update: impl Fn(u32) -> u32) -> u32 {
        let mut before = self.subs.load(Ordering::Relaxed);
        loop {
            let after = update(before);
            match self
                .subs
                .compare_exchange(before, after, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    shared.update_subscriptions(before, after);
                    return after;
                }
                Err(actual) => before = actual,
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.closing.notify_one();
            self.wake.notify_one();
        }
    }

    /// Queue a line. Never blocks: when the queue is full the oldest
    /// coalescable event goes; if there is none, the client is not reading
    /// at all and is disconnected.
    pub fn push(&self, line: Line, coalescable: bool) {
        if self.is_closed() {
            return;
        }
        let mut outbox = self.outbox.lock();
        if outbox.items.len() >= QUEUE_LEN {
            match outbox.items.iter().position(|(_, c)| *c) {
                Some(i) => {
                    outbox.items.remove(i);
                    outbox.dropped += 1;
                    self.dropped_total.fetch_add(1, Ordering::Relaxed);
                }
                None => {
                    drop(outbox);
                    log::warn!("bridge: client {} stopped reading, dropping it", self.id);
                    self.close();
                    return;
                }
            }
        }
        outbox.items.push_back((line, coalescable));
        drop(outbox);
        self.wake.notify_one();
    }

    pub fn respond(&self, id: &Value, result: RpcResult) {
        self.push(encode(&protocol::response(id, result)), false);
    }

    #[cfg(test)]
    fn queued(&self) -> (usize, u64) {
        let outbox = self.outbox.lock();
        (outbox.items.len(), outbox.dropped)
    }
}

/// Context a method handler runs with.
#[derive(Clone)]
pub struct Ctx {
    pub shared: Arc<Shared>,
    pub client: Arc<Client>,
}

async fn serve(stream: UnixStream, client: Arc<Client>, shared: Arc<Shared>) {
    let (read, write) = stream.into_split();
    let writer = tokio::spawn(write_loop(write, client.clone()));
    let ctx = Ctx {
        shared: shared.clone(),
        client: client.clone(),
    };
    tokio::select! {
        _ = read_loop(read, &ctx) => {}
        _ = client.closing.notified() => {}
    }
    client.close();
    client.set_subscriptions(&shared, |_| 0);
    for (_, cancel) in client.in_flight.lock().drain() {
        let _ = cancel.send(());
    }
    shared.clients.lock().retain(|c| c.id != client.id);
    let _ = writer.await;
}

async fn write_loop(mut write: OwnedWriteHalf, client: Arc<Client>) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        {
            let mut outbox = client.outbox.lock();
            if outbox.dropped > 0 {
                let overflow = protocol::notification(
                    "event/overflow",
                    OverflowEvent {
                        dropped: outbox.dropped,
                    },
                );
                buf.extend_from_slice(&encode(&overflow));
                outbox.dropped = 0;
            }
            for (line, _) in outbox.items.drain(..) {
                buf.extend_from_slice(&line);
            }
        }
        if buf.is_empty() {
            if client.is_closed() {
                break;
            }
            client.wake.notified().await;
            continue;
        }
        if write.write_all(&buf).await.is_err() {
            break;
        }
        if buf.capacity() > 1 << 20 {
            buf = Vec::new();
        }
    }
    client.close();
    let _ = write.shutdown().await;
}

async fn read_loop(read: OwnedReadHalf, ctx: &Ctx) {
    let mut reader = BufReader::with_capacity(8 * 1024, read);
    let mut line = Vec::new();
    let permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    loop {
        let available = match reader.fill_buf().await {
            Ok([]) | Err(_) => return,
            Ok(buf) => buf,
        };
        let (taken, complete) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                line.extend_from_slice(&available[..i]);
                (i + 1, true)
            }
            None => {
                line.extend_from_slice(available);
                (available.len(), false)
            }
        };
        reader.consume(taken);
        if line.len() > MAX_LINE {
            log::warn!("bridge: client {} sent a line over 8 MiB", ctx.client.id);
            return;
        }
        if complete {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !line.iter().all(u8::is_ascii_whitespace) {
                handle_message(&line, ctx, &permits).await;
            }
            line.clear();
            if line.capacity() > 1 << 20 {
                line = Vec::new();
            }
        }
    }
}

async fn handle_message(line: &[u8], ctx: &Ctx, permits: &Arc<Semaphore>) {
    let message: Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(err) => {
            let err = RpcError::new(ErrorCode::ParseError, format!("parse error: {err}"));
            return ctx.client.respond(&Value::Null, Err(err));
        }
    };
    let Value::Object(mut message) = message else {
        let err = RpcError::new(ErrorCode::InvalidRequest, "not a request object");
        return ctx.client.respond(&Value::Null, Err(err));
    };
    let id = message.remove("id");
    let invalid = |msg: &str, id: Option<&Value>| {
        let err = RpcError::new(ErrorCode::InvalidRequest, msg);
        ctx.client.respond(id.unwrap_or(&Value::Null), Err(err));
    };
    if let Some(id) = &id {
        if !(id.is_string() || id.is_number()) {
            return invalid("id must be a number or a string", None);
        }
    }
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return invalid("jsonrpc must be \"2.0\"", id.as_ref());
    }
    let Some(method) = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        if id.is_some() && (message.contains_key("result") || message.contains_key("error")) {
            // A response to nothing we asked; ignore it.
            return;
        }
        return invalid("method must be a string", id.as_ref());
    };
    let params = message.remove("params").unwrap_or(Value::Null);

    if method == "$/cancel" {
        if let Some(target) = params.get("id") {
            if let Some(cancel) = ctx.client.in_flight.lock().remove(&target.to_string()) {
                let _ = cancel.send(());
            }
        }
        if let Some(id) = id {
            ctx.client.respond(&id, Ok(serde_json::json!({})));
        }
        return;
    }
    let Some(id) = id else {
        // Unknown notifications are ignored, as JSON-RPC requires.
        return;
    };
    if !(params.is_object() || params.is_null()) {
        let err = RpcError::invalid_params("params", "params must be an object");
        return ctx.client.respond(&id, Err(err));
    }
    let Some(handler) = dispatch::lookup(&method) else {
        return ctx
            .client
            .respond(&id, Err(RpcError::method_not_found(&method)));
    };

    let Ok(permit) = permits.clone().acquire_owned().await else {
        return;
    };
    let key = id.to_string();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    ctx.client.in_flight.lock().insert(key.clone(), cancel_tx);
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let call = handler(ctx.clone(), params);
        let result = tokio::select! {
            result = call => result,
            Ok(()) = cancel_rx => Err(RpcError::new(ErrorCode::Cancelled, "request cancelled")),
        };
        ctx.client.in_flight.lock().remove(&key);
        ctx.client.respond(&id, result);
        drop(permit);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: u32) -> Line {
        encode(&serde_json::json!({ "n": n }))
    }

    #[test]
    fn overflow_drops_oldest_coalescable() {
        let client = Client::new(1);
        client.push(line(0), false);
        for n in 1..QUEUE_LEN as u32 {
            client.push(line(n), true);
        }
        assert_eq!(client.queued(), (QUEUE_LEN, 0));
        client.push(line(9999), false);
        assert_eq!(client.queued(), (QUEUE_LEN, 1));
        let outbox = client.outbox.lock();
        // the non-coalescable head survives; the first coalescable went
        assert_eq!(&*outbox.items[0].0, &*line(0));
        assert_eq!(&*outbox.items[1].0, &*line(2));
        assert_eq!(&*outbox.items.back().unwrap().0, &*line(9999));
    }

    #[test]
    fn full_of_responses_disconnects() {
        let client = Client::new(1);
        for n in 0..QUEUE_LEN as u32 {
            client.push(line(n), false);
        }
        assert!(!client.is_closed());
        client.push(line(0), true);
        assert!(client.is_closed());
        // a closed client ignores further pushes
        client.push(line(1), true);
        assert_eq!(client.queued(), (QUEUE_LEN, 0));
    }

    #[test]
    fn encoded_lines_have_one_newline() {
        let l = encode(&serde_json::json!({ "text": "a\nb" }));
        assert_eq!(l.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(l.last(), Some(&b'\n'));
    }
}
