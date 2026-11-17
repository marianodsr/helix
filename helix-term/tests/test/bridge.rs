//! bb bridge: drive a real `Application` over its socket.

use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context as _};
use helix_term::application::Application;
use helix_view::input::parse_macro;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

#[cfg(windows)]
use crossterm::event::{Event, KeyEvent};
#[cfg(not(windows))]
use termina::event::{Event, KeyEvent};

use super::*;

const WAIT: Duration = Duration::from_secs(5);

// --- harness -----------------------------------------------------------------

pub struct Bridge {
    pub app: Application,
    pub socket: PathBuf,
    registry: PathBuf,
    pub dir: tempfile::TempDir,
    keys: Keys,
    stream: UnboundedReceiverStream<std::io::Result<Event>>,
}

/// Sends key presses to the app, as if typed.
#[derive(Clone)]
pub struct Keys(UnboundedSender<std::io::Result<Event>>);

impl Keys {
    pub fn send(&self, keys: &str) -> anyhow::Result<()> {
        log::debug!("bridge test: keys {keys:?}");
        for key in parse_macro(keys)? {
            self.0.send(Ok(Event::Key(KeyEvent::from(key))))?;
        }
        Ok(())
    }

    /// Send a terminal event other than a key press (focus, resize).
    pub fn event(&self, event: Event) -> anyhow::Result<()> {
        self.0.send(Ok(event))?;
        Ok(())
    }
}

/// A short directory: socket paths are limited to about 100 bytes.
pub fn bridge_dir() -> anyhow::Result<tempfile::TempDir> {
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() > 40 {
        PathBuf::from("/tmp")
    } else {
        base
    };
    Ok(tempfile::Builder::new().prefix("hxb").tempdir_in(base)?)
}

impl Bridge {
    pub fn start(builder: AppBuilder) -> anyhow::Result<Bridge> {
        let dir = bridge_dir()?;
        Self::start_in(builder, dir)
    }

    pub fn start_in(builder: AppBuilder, dir: tempfile::TempDir) -> anyhow::Result<Bridge> {
        let bridge_dir = dir.path().join("bridge");
        Self::start_at(builder, dir, bridge_dir)
    }

    pub fn start_at(
        builder: AppBuilder,
        dir: tempfile::TempDir,
        bridge_dir: PathBuf,
    ) -> anyhow::Result<Bridge> {
        helix_term::bridge::set_test_dir(bridge_dir.clone());
        let app = builder.build()?;
        let pid = std::process::id();
        let socket = helix_term::bridge::socket_path()
            .unwrap_or_else(|| bridge_dir.join(format!("{pid}.sock")));
        let (tx, rx) = unbounded_channel();
        Ok(Bridge {
            app,
            socket,
            registry: bridge_dir.join(format!("{pid}.json")),
            dir,
            keys: Keys(tx),
            stream: UnboundedReceiverStream::new(rx),
        })
    }

    pub fn registry(&self) -> PathBuf {
        self.registry.clone()
    }

    /// Run the editor's main loop while `f` drives it from outside.
    pub async fn run<F, Fut, T>(&mut self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(Keys, PathBuf) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let mut task = tokio::spawn(f(self.keys.clone(), self.socket.clone()));
        tokio::select! {
            result = &mut task => result?,
            _ = self.app.event_loop(&mut self.stream) => {
                task.abort();
                bail!("the editor exited")
            }
        }
    }

    /// Quit the editor and run its shutdown.
    pub async fn finish(mut self) -> anyhow::Result<(tempfile::TempDir, PathBuf)> {
        self.keys.send("<esc>:qa!<ret>")?;
        tokio::time::timeout(WAIT, self.app.event_loop(&mut self.stream)).await?;
        let errs = self.app.close().await;
        ensure!(errs.is_empty(), "errors closing app: {errs:?}");
        Ok((self.dir, self.socket))
    }
}

/// Wait until the top of the editor is `what`: `"prompt"` (a prompt other
/// than the `:` line), `":"`, `"picker"`, `"popup"`. Typed commands open
/// prompts and pickers through the job queue, so keys sent right after
/// them can arrive first.
pub async fn until_overlay(what: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let overlays = helix_term::bridge::overlays().await;
        if overlays.last().map(String::as_str) == Some(what) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for a {what}: {overlays:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Wait until the status line reads `want`.
pub async fn until_status(want: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let status = helix_term::bridge::status().await;
        if status.as_deref() == Some(want) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "timed out waiting for status {want:?}: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// --- client ------------------------------------------------------------------

pub struct Client {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
    next_id: u64,
    /// Notifications read while waiting for something else.
    pub events: VecDeque<Value>,
}

impl Client {
    pub async fn connect(socket: &Path) -> anyhow::Result<Client> {
        let deadline = Instant::now() + WAIT;
        let stream = loop {
            match UnixStream::connect(socket).await {
                Ok(stream) => break stream,
                Err(err) if Instant::now() > deadline => {
                    return Err(err).context(format!("connecting to {socket:?}"))
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        let (read, write) = stream.into_split();
        Ok(Client {
            lines: BufReader::new(read).lines(),
            write,
            next_id: 0,
            events: VecDeque::new(),
        })
    }

    pub async fn send_raw(&mut self, line: &str) -> anyhow::Result<()> {
        self.write.write_all(line.as_bytes()).await?;
        self.write.write_all(b"\n").await?;
        Ok(())
    }

    pub async fn read(&mut self) -> anyhow::Result<Value> {
        let line = tokio::time::timeout(WAIT, self.lines.next_line())
            .await
            .context("timed out reading from the bridge")??
            .ok_or_else(|| anyhow!("the bridge closed the connection"))?;
        Ok(serde_json::from_str(&line)?)
    }

    /// Send a request without waiting; returns its id.
    pub async fn send(&mut self, method: &str, params: Value) -> anyhow::Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send_raw(&msg.to_string()).await?;
        Ok(id)
    }

    /// The response to `id`; notifications on the way are kept in `events`.
    pub async fn response(&mut self, id: u64) -> anyhow::Result<Value> {
        loop {
            let msg = self.read().await?;
            if msg.get("method").is_some() {
                self.events.push_back(msg);
            } else if msg["id"] == json!(id) {
                return Ok(msg);
            } else {
                bail!("unexpected message {msg}");
            }
        }
    }

    pub async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.send(method, params).await?;
        self.response(id)
            .await
            .with_context(|| format!("waiting for the answer to {method}"))
    }

    pub async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let response = self.request(method, params).await?;
        match response.get("result") {
            Some(result) => Ok(result.clone()),
            None => bail!("{method} failed: {response}"),
        }
    }

    /// Error code of a request expected to fail.
    pub async fn error(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let response = self.request(method, params).await?;
        response
            .get("error")
            .cloned()
            .ok_or_else(|| anyhow!("{method} should have failed: {response}"))
    }

    /// The next notification `method`, skipping (and keeping) others.
    pub async fn event(&mut self, method: &str) -> anyhow::Result<Value> {
        if let Some(i) = self.events.iter().position(|e| e["method"] == method) {
            return Ok(self.events.remove(i).unwrap()["params"].clone());
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = tokio::time::timeout(left, self.read())
                .await
                .with_context(|| format!("waiting for {method}"))?
                .with_context(|| format!("waiting for {method}"))?;
            if msg["method"] == method {
                return Ok(msg["params"].clone());
            }
            self.events.push_back(msg);
        }
    }

    /// Collect notifications for `dur`.
    pub async fn drain(&mut self, dur: Duration) -> anyhow::Result<Vec<Value>> {
        let deadline = tokio::time::Instant::now() + dur;
        let mut out: Vec<Value> = self.events.drain(..).collect();
        while let Ok(line) = tokio::time::timeout_at(deadline, self.lines.next_line()).await {
            match line? {
                Some(line) => out.push(serde_json::from_str(&line)?),
                None => break,
            }
        }
        Ok(out)
    }

    pub async fn hello(&mut self, name: &str) -> anyhow::Result<Value> {
        self.call(
            "bridge/hello",
            json!({ "client": name, "clientVersion": "0.0.1" }),
        )
        .await
    }

    pub async fn subscribe(&mut self, events: &[&str]) -> anyhow::Result<Value> {
        self.call("events/subscribe", json!({ "events": events }))
            .await
    }
}

fn methods(events: &[Value]) -> Vec<&str> {
    events.iter().filter_map(|e| e["method"].as_str()).collect()
}

fn pos(line: u64, col: u64) -> Value {
    json!({ "line": line, "col": col })
}

fn canonical(path: &Path) -> String {
    helix_stdx::path::canonicalize(path)
        .to_string_lossy()
        .into_owned()
}

// --- tests -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_registry_and_hello() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("package main\n")?;
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    let registry = bridge.registry();
    let (hello, socket) = bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            Ok((c.hello("test").await?, socket))
        })
        .await?;
    assert_eq!(hello["protocol"], 1);
    assert_eq!(hello["pid"], std::process::id());
    // every piece is there, in PROTOCOL.md's order
    assert_eq!(
        hello["capabilities"],
        json!([
            "editor",
            "reload",
            "locations",
            "views",
            "command",
            "diagnostics",
            "lsp",
            "applyEdits",
            "annotations",
            "reveal",
            "watch",
            "review"
        ])
    );
    assert_eq!(hello["commandPolicy"], "safe");
    assert!(hello["helixVersion"].as_str().unwrap().contains('.'));
    assert!(hello["workspaceRoot"].as_str().is_some());

    let reg: Value = serde_json::from_slice(&std::fs::read(&registry)?)?;
    assert_eq!(reg["protocol"], 1);
    assert_eq!(reg["pid"], std::process::id());
    assert_eq!(reg["socket"], socket.to_string_lossy().as_ref());
    assert!(reg["startedAt"].as_str().unwrap().ends_with('Z'));
    assert!(reg["env"].is_object());
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(socket.parent().unwrap()), 0o700);
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(&registry), 0o600);

    let (_dir, socket) = bridge.finish().await?;
    assert!(!socket.exists(), "socket removed on exit");
    assert!(!registry.exists(), "registry removed on exit");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_socket_from_same_pid_is_replaced() -> anyhow::Result<()> {
    let dir = bridge_dir()?;
    let bridge_dir = dir.path().join("bridge");
    std::fs::create_dir_all(&bridge_dir)?;
    let stale = bridge_dir.join(format!("{}.sock", std::process::id()));
    // a socket nobody listens on, as left by a crashed process
    drop(std::os::unix::net::UnixListener::bind(&stale)?);
    std::fs::write(stale.with_extension("json"), "{}")?;
    let mut bridge = Bridge::start_in(AppBuilder::new(), dir)?;
    let hello = bridge
        .run(|_, socket| async move { Client::connect(&socket).await?.hello("t").await })
        .await?;
    assert_eq!(hello["protocol"], 1);
    let reg: Value = serde_json::from_slice(&std::fs::read(bridge.registry())?)?;
    assert_eq!(reg["protocol"], 1, "registry rewritten");
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn long_cache_path_puts_the_socket_in_tmp() -> anyhow::Result<()> {
    let dir = bridge_dir()?;
    let bridge_dir = dir
        .path()
        .join("a-cache-directory-with-a-long-name/".repeat(3))
        .join("helix/bridge");
    let mut bridge = Bridge::start_at(AppBuilder::new(), dir, bridge_dir)?;
    let registry = bridge.registry();
    let socket = bridge.socket.clone();
    assert!(
        socket.to_string_lossy().starts_with("/tmp/helix-bridge-"),
        "fallback socket: {socket:?}"
    );
    let hello = bridge
        .run(|_, socket| async move { Client::connect(&socket).await?.hello("long").await })
        .await?;
    assert_eq!(hello["protocol"], 1);
    let reg: Value = serde_json::from_slice(&std::fs::read(&registry)?)?;
    assert_eq!(reg["socket"], socket.to_string_lossy().as_ref());
    let parent = socket.parent().unwrap();
    assert_eq!(
        std::fs::metadata(parent)?.permissions().mode() & 0o777,
        0o700
    );
    bridge.finish().await?;
    assert!(!socket.exists() && !registry.exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_by_config() -> anyhow::Result<()> {
    let mut config = helpers::test_config();
    config.editor.bridge.enable = false;
    let bridge = Bridge::start(AppBuilder::new().with_config(config))?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!bridge.socket.exists());
    assert!(helix_term::bridge::socket_path().is_none());
    bridge.finish().await?;
    Ok(())
}

/// Requests that only read do not make Helix draw a frame; the ones that
/// can change the screen still do, every time.
#[tokio::test(flavor = "multi_thread")]
async fn read_only_requests_do_not_repaint() -> anyhow::Result<()> {
    use helix_term::bridge::views_drawn;
    let file = helpers::temp_file_with_contents("one\ntwo\n")?;
    let path = file.path().to_string_lossy().into_owned();
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    let (quiet, repainted) = bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["selection", "documents"]).await?;
            // let the startup frames and the idle timer pass
            tokio::time::sleep(Duration::from_millis(300)).await;
            let before = views_drawn();
            for _ in 0..20 {
                for method in [
                    "bridge/hello",
                    "editor/state",
                    "editor/selection",
                    "view/list",
                    "diagnostics/get",
                    "lsp/servers",
                ] {
                    c.call(method, json!({})).await?;
                }
                c.call("editor/text", json!({ "path": path })).await?;
                c.error("lsp/query", json!({ "kind": "hover", "path": path, "position": { "line": 1, "col": 1 } }))
                    .await?;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let quiet = views_drawn() - before;
            let before = views_drawn();
            for i in 0..20 {
                c.call("editor/status", json!({ "message": format!("m{i}") }))
                    .await?;
                c.call("editor/select", json!({ "ranges": [{ "start": { "line": 1 + i % 2, "col": 1 }, "end": { "line": 1 + i % 2, "col": 1 } }] }))
                    .await?;
            }
            // the job answers before the main loop draws its frame
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok((quiet, views_drawn() - before))
        })
        .await?;
    // a stray frame from a timer is allowed, a frame per request is not
    assert!(quiet <= 2, "160 read-only requests drew {quiet} frames");
    assert!(
        repainted >= 40,
        "40 changing requests drew {repainted} frames"
    );
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn state_open_text_selection_status() -> anyhow::Result<()> {
    let a = helpers::temp_file_with_contents("aé🦀\nline two\r\nthree\n")?;
    let b = helpers::temp_file_with_contents("package b\n\nfunc B() {}\n")?;
    let missing = a.path().with_extension("missing");
    let (a_path, b_path) = (canonical(a.path()), canonical(b.path()));
    let mut bridge = Bridge::start(
        AppBuilder::new().with_file(a.path(), Some(helix_core::Position::new(1, 2))),
    )?;
    let (ap, bp) = (a_path.clone(), b_path.clone());
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let state = c.call("editor/state", json!({})).await?;
            assert_eq!(state["mode"], "normal");
            let focused = &state["focused"];
            assert_eq!(focused["path"], ap.as_str());
            assert_eq!(focused["modified"], false);
            // Helix position (1, 2) is 0-based: line 2, col 3 here; a cursor
            assert_eq!(
                focused["selections"],
                json!([{ "start": pos(2, 3), "end": pos(2, 3) }])
            );
            assert_eq!(focused["primary"], 0);
            assert_eq!(focused["visible"]["firstLine"], 1);
            assert!(focused["visible"]["lastLine"].as_u64().unwrap() >= 4);
            let docs = state["documents"].as_array().unwrap();
            assert_eq!(docs.len(), 1);
            let a_id = focused["docId"].clone();

            // cursor selection: text is the line, without its line ending
            let sel = c.call("editor/selection", json!({})).await?;
            assert_eq!(sel["ranges"][0]["text"], "line two");
            assert_eq!(sel["truncated"], false);

            // open b at a range and check the selection text
            let opened = c
                .call(
                    "editor/open",
                    json!({ "path": bp, "line": 3, "col": 6, "endLine": 3, "endCol": 9 }),
                )
                .await?;
            assert_eq!(opened["path"], bp.as_str());
            assert_eq!(opened["deferred"], false);
            assert!(opened["viewId"].is_u64());
            let sel = c.call("editor/selection", json!({})).await?;
            assert_eq!(sel["path"], bp.as_str());
            assert_eq!(sel["ranges"][0]["text"], "B()");
            assert_eq!(
                (&sel["ranges"][0]["start"], &sel["ranges"][0]["end"]),
                (&pos(3, 6), &pos(3, 9))
            );
            // endLine without endCol takes that whole line, line ending included
            c.call(
                "editor/open",
                json!({ "path": bp, "line": 1, "endLine": 1 }),
            )
            .await?;
            let sel = c.call("editor/selection", json!({})).await?;
            assert_eq!(sel["ranges"][0]["text"], "package b\n");
            assert_eq!(sel["ranges"][0]["end"], pos(2, 1));

            // opening the already-open file switches back to it
            let again = c
                .call("editor/open", json!({ "path": ap, "line": 1, "col": 3 }))
                .await?;
            assert_eq!(again["docId"], a_id);
            let state = c.call("editor/state", json!({})).await?;
            assert_eq!(state["focused"]["path"], ap.as_str());
            assert_eq!(
                state["focused"]["selections"][0]["start"],
                pos(1, 3),
                "col counts chars: the crab"
            );

            // background load does not move the user
            let bg = c
                .call(
                    "editor/open",
                    json!({ "path": bp, "focus": false, "line": 2 }),
                )
                .await?;
            assert!(bg["viewId"].is_null());
            let state = c.call("editor/state", json!({})).await?;
            assert_eq!(state["focused"]["path"], ap.as_str());
            assert_eq!(state["documents"].as_array().unwrap().len(), 2);

            // a vertical split is a new view
            let split = c
                .call(
                    "editor/open",
                    json!({ "path": bp, "split": "vertical", "note": "look here" }),
                )
                .await?;
            assert_ne!(split["viewId"], opened["viewId"]);

            // text: open buffer, range, by docId, and from disk
            let text = c.call("editor/text", json!({ "docId": a_id })).await?;
            assert_eq!(text["text"], "aé🦀\nline two\r\nthree\n");
            assert_eq!(text["modified"], false);
            assert!(text["version"].is_number());
            let text = c
                .call(
                    "editor/text",
                    json!({ "path": ap, "range": { "start": pos(1, 3), "end": pos(2, 5) } }),
                )
                .await?;
            assert_eq!(text["text"], "🦀\nline");
            let disk = c
                .call("editor/text", json!({ "path": "README.md" }))
                .await?;
            assert!(disk["version"].is_null());
            assert!(disk["text"].as_str().unwrap().contains("Helix"));
            assert!(disk["path"].as_str().unwrap().ends_with("/README.md"));

            // errors
            let err = c.error("editor/open", json!({ "path": missing })).await?;
            assert_eq!(err["code"], 1001);
            let err = c.error("editor/text", json!({ "path": missing })).await?;
            assert_eq!(err["code"], 1001);
            let err = c.error("editor/text", json!({ "docId": 999 })).await?;
            assert_eq!(err["code"], 1001);
            let err = c.error("editor/text", json!({})).await?;
            assert_eq!(err["code"], -32602);
            assert_eq!(err["data"]["field"], "path");
            let err = c.error("editor/open", json!({ "line": 1 })).await?;
            assert_eq!(err["data"]["field"], "path");
            let err = c
                .error("editor/open", json!({ "path": ap, "interrupt": "never" }))
                .await?;
            assert_eq!(err["data"]["field"], "interrupt");

            let ok = c
                .call(
                    "editor/status",
                    json!({ "message": "hello from bb", "severity": "warning" }),
                )
                .await?;
            assert_eq!(ok, json!({}));
            Ok(())
        })
        .await?;
    let (status, severity) = bridge.app.editor.get_status().unwrap();
    assert_eq!(status, "hello from bb");
    assert_eq!(*severity, helix_core::diagnostic::Severity::Warning);
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn protocol_errors() -> anyhow::Result<()> {
    let mut bridge = Bridge::start(AppBuilder::new())?;
    bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.send_raw("{not json").await?;
            let r = c.read().await?;
            assert_eq!(r["error"]["code"], -32700);
            assert_eq!(r["id"], Value::Null);
            c.send_raw("[1, 2]").await?;
            assert_eq!(c.read().await?["error"]["code"], -32600);
            c.send_raw(r#"{"id": 5, "method": "editor/state"}"#).await?;
            let r = c.read().await?;
            assert_eq!(
                (r["error"]["code"].clone(), r["id"].clone()),
                (json!(-32600), json!(5))
            );
            c.send_raw(r#"{"jsonrpc": "2.0", "id": {}, "method": "editor/state"}"#)
                .await?;
            assert_eq!(c.read().await?["error"]["code"], -32600);
            c.send_raw(r#"{"jsonrpc": "2.0", "id": 6, "method": 7}"#)
                .await?;
            assert_eq!(c.read().await?["error"]["code"], -32600);
            // string ids work and come back as strings
            c.send_raw(r#"{"jsonrpc": "2.0", "id": "abc", "method": "editor/state"}"#)
                .await?;
            let r = c.read().await?;
            assert_eq!(r["id"], "abc");
            assert!(r["result"]["mode"].is_string());

            assert_eq!(c.error("no/such", json!({})).await?["code"], -32601);
            assert_eq!(c.error("editor/state", json!([1])).await?["code"], -32602);
            let err = c
                .error(
                    "events/subscribe",
                    json!({ "events": ["selection", "bogus"] }),
                )
                .await?;
            assert_eq!(err["code"], -32602);
            assert_eq!(err["data"]["field"], "events");

            // notifications get no answer, so the next response is the state
            c.send_raw(r#"{"jsonrpc": "2.0", "method": "editor/state"}"#)
                .await?;
            c.send_raw(r#"{"jsonrpc": "2.0", "method": "$/cancel", "params": {"id": 999}}"#)
                .await?;
            let state = c.request("editor/state", json!({})).await?;
            assert!(state["result"].is_object());

            // a line over 8 MiB closes the connection
            let mut big = String::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"editor/status","params":{"message":""#,
            );
            big.push_str(&"x".repeat(8 * 1024 * 1024 + 10));
            big.push_str(r#""}}"#);
            let _ = c.send_raw(&big).await;
            let closed = tokio::time::timeout(WAIT, c.lines.next_line()).await?;
            assert!(matches!(closed, Ok(None) | Err(_)), "{closed:?}");

            // ... and the server keeps serving others
            let mut d = Client::connect(&socket).await?;
            d.hello("after").await?;
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_and_timeout() -> anyhow::Result<()> {
    let mut bridge = Bridge::start(AppBuilder::new())?;
    let socket = bridge.socket.clone();
    // the main loop is not running here, so editor requests wait
    let mut c = Client::connect(&socket).await?;
    let id = c.send("editor/state", json!({})).await?;
    c.send_raw(
        &json!({ "jsonrpc": "2.0", "method": "$/cancel", "params": { "id": id } }).to_string(),
    )
    .await?;
    let r = c.response(id).await?;
    assert_eq!(r["error"]["code"], 1008, "{r}");

    let started = Instant::now();
    let r = c.request("editor/selection", json!({})).await?;
    assert_eq!(r["error"]["code"], 1005, "{r}");
    assert!(started.elapsed() >= Duration::from_millis(1900));

    // once the loop runs again, the same client is served
    let state = bridge
        .run(|_, _| async move { c.call("editor/state", json!({})).await })
        .await?;
    assert_eq!(state["mode"], "normal");
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn events_selection_documents_mode() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("one\ntwo\nthree\nfour\nfive\n")?;
    let other = helpers::temp_file_with_contents("other\n")?;
    let (path, other_path) = (canonical(file.path()), canonical(other.path()));
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let subs = c
                .subscribe(&[
                    "selection",
                    "documents",
                    "mode",
                    "diagnostics",
                    "lsp",
                    "externalChange",
                ])
                .await?;
            assert_eq!(
                subs["subscribed"],
                json!([
                    "selection",
                    "documents",
                    "diagnostics",
                    "mode",
                    "lsp",
                    "externalChange"
                ])
            );

            keys.send("j")?;
            let sel = c.event("event/selection").await?;
            assert_eq!(sel["path"], path.as_str());
            assert_eq!(
                sel["ranges"],
                json!([{ "start": pos(2, 1), "end": pos(2, 1) }])
            );

            // a burst: the first change at once, the rest as one event a
            // window later, with the final state. The five moves are fed in
            // one turn of the main loop (`editor/command` keys), so how fast
            // a loaded machine handles key presses cannot spread them over
            // several windows.
            c.drain(Duration::from_millis(120)).await?;
            let id = c
                .send(
                    "editor/command",
                    json!({ "keys": "jjjkk", "interrupt": "always" }),
                )
                .await?;
            let first = c.event("event/selection").await?;
            let more = c.drain(Duration::from_millis(250)).await?;
            ensure!(
                more.iter()
                    .any(|m| m["id"] == id && m["result"]["deferred"] == false),
                "editor/command ran: {more:?}"
            );
            let sels: Vec<_> = std::iter::once(first)
                .chain(
                    more.iter()
                        .filter(|e| e["method"] == "event/selection")
                        .map(|e| e["params"].clone()),
                )
                .collect();
            assert!(sels.len() <= 2, "coalesced: {sels:?}");
            assert_eq!(
                sels.last().unwrap()["ranges"],
                json!([{ "start": pos(3, 1), "end": pos(3, 1) }])
            );

            keys.send("i")?;
            assert_eq!(c.event("event/mode").await?["mode"], "insert");
            keys.send("abc")?;
            let changed = c.event("event/documentChanged").await?;
            assert_eq!(changed["path"], path.as_str());
            assert_eq!(changed["modified"], true);
            let version = changed["version"].as_i64().unwrap();
            keys.send("<esc>")?;
            assert_eq!(c.event("event/mode").await?["mode"], "normal");
            keys.send("v")?;
            assert_eq!(c.event("event/mode").await?["mode"], "select");
            keys.send("<esc>")?;
            assert_eq!(c.event("event/mode").await?["mode"], "normal");

            keys.send(":w<ret>")?;
            let saved = c.event("event/documentSaved").await?;
            assert_eq!(saved["path"], path.as_str());
            assert!(saved["version"].as_i64().unwrap() >= version);

            // the first edit after a save is modified; undo back to the
            // saved text is not
            keys.send("ix<esc>")?;
            let ch = c.event("event/documentChanged").await?;
            assert_eq!(ch["modified"], true, "{ch}");
            keys.send("u")?;
            loop {
                let ch = c.event("event/documentChanged").await?;
                if ch["modified"] == false {
                    break;
                }
            }

            c.drain(Duration::from_millis(150)).await?;
            // the path is pasted, not typed: the command line completes
            // file names on every character typed, which on a loaded
            // machine (a debug build, a temp directory of hundreds of
            // files) can take longer than the wait for the event
            keys.send(":o ")?;
            keys.event(Event::Paste(other_path.clone()))?;
            keys.send("<ret>")?;
            let opened = c.event("event/documentOpened").await?;
            assert_eq!(opened["path"], other_path.as_str());
            let focus = c.event("event/focus").await?;
            assert_eq!(focus["path"], other_path.as_str());
            let sel = c.event("event/selection").await?;
            assert_eq!(sel["path"], other_path.as_str());

            keys.send(":bc<ret>")?;
            let closed = c.event("event/documentClosed").await?;
            assert_eq!(closed["path"], other_path.as_str());
            let focus = c.event("event/focus").await?;
            assert_eq!(focus["path"], path.as_str());
            // a focus change is followed by the new pane's selection; take it
            // so it cannot arrive after the unsubscribe below
            assert_eq!(c.event("event/selection").await?["path"], path.as_str());

            // after unsubscribing, nothing more arrives
            let left = c
                .call(
                    "events/unsubscribe",
                    json!({ "events": ["selection", "mode"] }),
                )
                .await?;
            assert_eq!(
                left["subscribed"],
                json!(["documents", "diagnostics", "lsp", "externalChange"])
            );
            keys.send("jki<esc>")?;
            let rest = c.drain(Duration::from_millis(300)).await?;
            assert!(
                !methods(&rest)
                    .iter()
                    .any(|m| *m == "event/selection" || *m == "event/mode"),
                "{rest:?}"
            );
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

/// Bindings exactly as a user would write them in config.toml.
const USER_KEYS: &str = r#"
[keys.normal.space.b]
s = ":bb-send"
c = ":bb-comment"
C = ":bb-comments-send"

[keys.select]
C = ":bb-comment"
S = ":bb-send! from select mode"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn send_commands_and_bindings() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("fn main() {\n    let x = 1;\n}\n")?;
    let path = canonical(file.path());
    let mut config = helpers::test_config();
    let raw: helix_term::config::ConfigRaw = toml::from_str(USER_KEYS)?;
    helix_term::keymap::merge_keys(&mut config.keys, raw.keys.unwrap());
    let mut bridge = Bridge::start(
        AppBuilder::new()
            .with_config(config)
            .with_file(file.path(), None),
    )?;

    // with nobody subscribed, the commands fail and change nothing
    let p = path.clone();
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            keys.send(":bb-send hi<ret>")?;
            let state = c.call("editor/state", json!({})).await?;
            assert_eq!(state["focused"]["path"], p.as_str());
            Ok(())
        })
        .await?;
    let (status, severity) = bridge.app.editor.get_status().unwrap();
    assert!(status.contains("No bb client"), "{status}");
    assert_eq!(*severity, helix_core::diagnostic::Severity::Error);

    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["send"]).await?;

            keys.send("jx:bb-send don't touch 50%% of this<ret>")?;
            let ev = c.event("event/send").await?;
            assert_eq!(ev["kind"], "send");
            assert_eq!(ev["note"], "don't touch 50% of this");
            assert_eq!(ev["sendNow"], false);
            assert_eq!(ev["path"], path.as_str());
            assert_eq!(ev["languageId"], Value::Null);
            assert_eq!(ev["ranges"][0]["text"], "    let x = 1;\n");
            assert_eq!(ev["ranges"][0]["start"], pos(2, 1));
            assert_eq!(ev["ranges"][0]["end"], pos(3, 1));
            assert_eq!(ev["truncated"], false);

            keys.send(":bb-send!<ret>")?;
            let ev = c.event("event/send").await?;
            assert_eq!(
                (
                    ev["kind"].clone(),
                    ev["sendNow"].clone(),
                    ev["note"].clone()
                ),
                (json!("send"), json!(true), json!(""))
            );

            keys.send(":bb-ask why is x 1?<ret>")?;
            let ev = c.event("event/send").await?;
            assert_eq!(
                (ev["kind"].clone(), ev["sendNow"].clone()),
                (json!("ask"), json!(true))
            );
            assert_eq!(ev["note"], "why is x 1?");

            keys.send(":bb-comment rename x<ret>")?;
            let ev = c.event("event/send").await?;
            assert_eq!(
                (
                    ev["kind"].clone(),
                    ev["note"].clone(),
                    ev["sendNow"].clone()
                ),
                (json!("comment"), json!("rename x"), json!(false))
            );

            // the prompt: Enter emits, Escape cancels
            keys.send(":bb-comment<ret>")?;
            until_overlay("prompt").await?;
            keys.send("typed in the prompt: 100% \"literal\"<ret>")?;
            let ev = c.event("event/send").await?;
            assert_eq!(ev["note"], "typed in the prompt: 100% \"literal\"");
            keys.send(":bb-comment<ret>")?;
            until_overlay("prompt").await?;
            keys.send("never mind<esc>")?;
            let rest = c.drain(Duration::from_millis(300)).await?;
            assert!(methods(&rest).is_empty(), "escape sends nothing: {rest:?}");

            keys.send(":bb-comments-send<ret>")?;
            let ev = c.event("event/send").await?;
            assert_eq!(ev["kind"], "flush");
            assert_eq!(ev["ranges"], json!([]));

            // the same commands from the user's own bindings, normal mode
            keys.send("<space>bs")?;
            assert_eq!(c.event("event/send").await?["kind"], "send");
            keys.send("<space>bc")?;
            until_overlay("prompt").await?;
            keys.send("from a binding<ret>")?;
            let ev = c.event("event/send").await?;
            assert_eq!(
                (ev["kind"].clone(), ev["note"].clone()),
                (json!("comment"), json!("from a binding"))
            );
            keys.send("<space>bC")?;
            assert_eq!(c.event("event/send").await?["kind"], "flush");

            // ... and select mode: two lines selected
            keys.send("gg")?;
            keys.send("vjC")?;
            until_overlay("prompt").await?;
            keys.send("two lines<ret>")?;
            let ev = c.event("event/send").await?;
            assert_eq!(ev["note"], "two lines");
            assert_eq!(ev["ranges"][0]["start"], pos(1, 1));
            assert_eq!(ev["ranges"][0]["end"], pos(2, 2));
            assert_eq!(ev["ranges"][0]["text"], "fn main() {\n ");
            keys.send("S")?;
            let ev = c.event("event/send").await?;
            assert_eq!(
                (ev["note"].clone(), ev["sendNow"].clone()),
                (json!("from select mode"), json!(true))
            );
            Ok(())
        })
        .await?;
    let (status, _) = bridge.app.editor.get_status().unwrap();
    assert_eq!(status, "Sent to bb");
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_and_one_that_stops_reading() -> anyhow::Result<()> {
    let text: String = (1..=400).map(|i| format!("line {i}\n")).collect();
    let file = helpers::temp_file_with_contents(&text)?;
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    bridge
        .run(move |keys, socket| async move {
            let mut a = Client::connect(&socket).await?;
            let mut b = Client::connect(&socket).await?;
            a.hello("a").await?;
            b.hello("b").await?;
            a.subscribe(&["selection", "mode"]).await?;
            b.subscribe(&["selection"]).await?;

            keys.send("j")?;
            let ea = a.event("event/selection").await?;
            let eb = b.event("event/selection").await?;
            assert_eq!(ea, eb);
            keys.send("i")?;
            assert_eq!(a.event("event/mode").await?["mode"], "insert");
            keys.send("<esc>")?;
            let rest = b.drain(Duration::from_millis(200)).await?;
            assert!(
                !methods(&rest).contains(&"event/mode"),
                "b did not ask for mode"
            );

            // b goes away; a is unaffected
            drop(b);
            keys.send("j")?;
            a.event("event/selection").await?;

            // 1000 key presses and back to the top, until a sees it
            async fn burst(keys: &Keys, a: &mut Client) -> anyhow::Result<Duration> {
                let started = Instant::now();
                for _ in 0..50 {
                    keys.send("jjjjjjjjjjkkkkkkkkkk")?;
                }
                keys.send("2ggk")?;
                loop {
                    let sel = a.event("event/selection").await?;
                    if sel["ranges"][0]["start"] == pos(1, 1) {
                        return Ok(started.elapsed());
                    }
                }
            }
            keys.send("2gg")?;
            a.drain(Duration::from_millis(100)).await?;
            let alone = burst(&keys, &mut a).await?;

            // c subscribes to everything and never reads again: the editor
            // keeps up as it did without it, and a keeps getting events
            let mut c = Client::connect(&socket).await?;
            c.hello("slow").await?;
            c.subscribe(&["selection", "documents", "mode", "send"])
                .await?;
            keys.send("2gg")?;
            a.drain(Duration::from_millis(100)).await?;
            let stuck = burst(&keys, &mut a).await?;
            assert!(
                stuck < alone * 3 + Duration::from_millis(500),
                "1000 keys took {stuck:?} with a stuck client, {alone:?} without"
            );

            // flood c with requests it never reads the answers to: once its
            // queue is full of responses, the bridge drops it
            let mut sent = 0;
            let flood = async {
                loop {
                    c.send("bridge/hello", json!({})).await?;
                    sent += 1;
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            };
            let result = tokio::time::timeout(Duration::from_secs(10), flood).await;
            assert!(
                matches!(result, Ok(Err(_))),
                "the bridge closed c after {sent} requests"
            );
            assert!(sent > 1024, "{sent}");

            // a still works, and :bb-status sees only a
            keys.send(":bb-status<ret>")?;
            a.call("editor/state", json!({})).await?;
            Ok(())
        })
        .await?;
    let (status, _) = bridge.app.editor.get_status().unwrap();
    assert!(
        status.contains("1 client: a 0.0.1 [selection mode]"),
        "{status}"
    );
    bridge.finish().await?;
    Ok(())
}
