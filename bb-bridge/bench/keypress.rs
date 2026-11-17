//! Key-press benchmark (bb-bridge/ARCHITECTURE.md, "Performance targets").
//!
//! Replays a fixed key sequence over a large Go file in Helix's integration
//! harness and reports the time per key. Variants:
//!
//! - `Off`: the bridge not started (on the base commit: upstream Helix, this
//!   file copied there with the lines between
//!   the `bridge:begin` / `bridge:end` markers removed);
//! - `NoClient`: the bridge listening, nobody connected;
//! - `Subscribed`: one client subscribed to every event kind, reading all;
//! - `Diagnostics` / `Annotations` (step 2d): 500 warnings shown at the end
//!   of their lines, or 500 bridge annotations under them, every 8 lines
//!   over the first 4000 (where the key sequence moves), bridge listening.
//! - `DiffGutter` / `InlineDiff` (step 5): a diff base with 500 hunks over
//!   the same lines (changed, added and removed lines in turn), with
//!   Helix's own gutter only, or with the bridge's inline diff drawn too.
//!
//! `KEYPRESS_ONLY=InlineDiff,Diagnostics` runs only those rows.
//! `KEYPRESS_DEBUG=1` prints where each run ended and how many panes it drew.
//!
//! Run with `bb-bridge/bench/keypress.sh`; it is an ignored test.

use std::time::{Duration, Instant};

use helix_term::application::Application;
use helix_view::input::parse_macro;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_stream::wrappers::UnboundedReceiverStream;

#[cfg(windows)]
use crossterm::event::{Event, KeyEvent};
#[cfg(not(windows))]
use termina::event::{Event, KeyEvent};

use super::helpers::{test_config, AppBuilder};

/// Moving and selecting: the key-press path the bridge hooks sit on.
const MOVE: &str = "jjjjjwwwbbbe;xkkjjjvjjjhhh<esc>jjjjjkkkkkGgg5j";
const MOVE_PASSES: usize = 200;
/// Editing, then undo, so the document ends where it started. Each edit of a
/// 20k-line Go file costs upstream Helix a few hundred ms in tree-sitter
/// (`Syntax::update`), so this row is short and reported on its own.
const EDIT: &str = "iabc<esc>uofoo := 1<esc>u";
const EDIT_PASSES: usize = 6;
const WARMUP_PASSES: usize = 5;
const ROUNDS: usize = 5;

/// `KEYPRESS_PASSES` / `KEYPRESS_ROUNDS` override the defaults, for a quick look.
fn setting(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

type Keys = UnboundedSender<std::io::Result<Event>>;

/// About 20k lines of Go. No comments or string literals in the bodies:
/// each one is a tree-sitter injection layer, and thousands of them make
/// every edit cost seconds in upstream Helix, which would drown what this
/// benchmark is for (key handling, rendering, the bridge hooks).
fn go_source() -> String {
    let mut out = String::from("package main\n\n");
    for i in 0..4000 {
        out.push_str(&format!(
            "func f{i}(x int) int {{\n\ty := x * {i}\n\treturn y + {i}\n}}\n\n"
        ));
    }
    out
}

fn send(keys: &Keys, text: &str, times: usize) -> usize {
    let events = parse_macro(text).unwrap();
    for _ in 0..times {
        for key in &events {
            keys.send(Ok(Event::Key(KeyEvent::from(*key)))).unwrap();
        }
    }
    events.len() * times
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Variant {
    Off,
    // bridge:begin
    NoClient,
    Subscribed,
    Diagnostics,
    Annotations,
    DiffGutter,
    InlineDiff,
    // bridge:end
}

// bridge:begin
/// Step 2d rows: the same number of diagnostics or annotations.
fn mark_lines(app: &mut Application, variant: Variant) {
    use helix_core::diagnostic::{Diagnostic, DiagnosticProvider, Severity};
    use helix_view::annotations::bridge::{Item, Kind};
    let message = "this line has something to say about the code it marks";
    let doc = helix_view::doc_mut!(app.editor);
    let text = doc.text().slice(..);
    let lines = (2..4000).step_by(8);
    match variant {
        Variant::Diagnostics => {
            let provider = DiagnosticProvider::Lsp {
                server_id: Default::default(),
                identifier: None,
            };
            let diagnostics: Vec<_> = lines
                .map(|line| {
                    let start = text.line_to_char(line) + 1;
                    Diagnostic {
                        range: helix_core::diagnostic::Range {
                            start,
                            end: start + 4,
                        },
                        ends_at_word: false,
                        starts_at_word: false,
                        zero_width: false,
                        line,
                        message: message.into(),
                        severity: Some(Severity::Warning),
                        code: None,
                        provider: provider.clone(),
                        tags: Vec::new(),
                        source: None,
                        data: None,
                    }
                })
                .collect();
            doc.replace_diagnostics(diagnostics, &[], Some(&provider));
        }
        Variant::DiffGutter | Variant::InlineDiff => {
            // the base differs from the text near every eighth line
            let mut base = String::new();
            for (i, line) in text.lines().enumerate() {
                let line = line.to_string();
                if i >= 2 && i < 4000 && (i - 2) % 8 == 0 {
                    let kinds = if std::env::var_os("KEYPRESS_ADDED_ONLY").is_some() {
                        1
                    } else {
                        3
                    };
                    match ((i / 8) % kinds) + (kinds == 1) as usize {
                        0 => base.push_str(&line.replace("return", "return 0 +")),
                        1 => {} // added in the text
                        _ => {
                            base.push_str(&line);
                            base.push_str("\tremoved := 1\n");
                        }
                    }
                } else {
                    base.push_str(&line);
                }
            }
            doc.set_diff_base(base.into_bytes());
            helix_view::annotations::bridge::set_inline_diff(variant == Variant::InlineDiff);
        }
        Variant::Annotations => {
            let items = lines
                .map(|line| {
                    let body = message.to_owned();
                    Item::new(text, line, line, body, Kind::Comment, None, None, false)
                })
                .collect();
            doc.bridge_annotations.set("bench", items);
        }
        _ => {}
    }
}
// bridge:end

/// Build an app on its own runtime, replay `keys` `passes` times, return
/// the time taken and the number of key presses.
fn run(variant: Variant, file: &std::path::Path, keys: &str, passes: usize) -> (Duration, usize) {
    let keys = keys.to_owned();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let mut config = test_config();
        config.editor.idle_timeout = Duration::from_millis(10);
        // bridge:begin
        if variant == Variant::Diagnostics {
            config.editor.end_of_line_diagnostics =
                helix_view::annotations::diagnostics::DiagnosticFilter::Enable(
                    helix_core::diagnostic::Severity::Hint,
                );
        }
        // bridge:end
        // bridge:begin
        let dir = tempfile::Builder::new()
            .prefix("hxk")
            .tempdir_in("/tmp")
            .unwrap();
        if variant != Variant::Off {
            helix_term::bridge::set_test_dir(dir.path().join("bridge"));
        }
        // bridge:end
        let mut app = AppBuilder::new()
            .with_config(config)
            .with_file(file, None)
            .build()
            .unwrap();
        let (tx, rx) = unbounded_channel();
        let mut stream = UnboundedReceiverStream::new(rx);
        // bridge:begin
        mark_lines(&mut app, variant);
        // bridge:end

        // bridge:begin
        let received = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        if variant == Variant::Subscribed {
            let socket = helix_term::bridge::socket_path().unwrap();
            let received = received.clone();
            let client = async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                let stream = loop {
                    match tokio::net::UnixStream::connect(&socket).await {
                        Ok(s) => break s,
                        Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
                    }
                };
                let (read, mut write) = stream.into_split();
                let all = r#"{"jsonrpc":"2.0","id":1,"method":"events/subscribe","params":{"events":["selection","documents","diagnostics","mode","send","lsp","externalChange"]}}"#;
                write.write_all(all.as_bytes()).await.unwrap();
                write.write_all(b"\n").await.unwrap();
                let mut lines = BufReader::new(read).lines();
                // the answer to subscribe, then events until the end
                lines.next_line().await.unwrap().unwrap();
                tokio::spawn(async move {
                    let _write = write;
                    while let Ok(Some(_)) = lines.next_line().await {
                        received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            };
            let mut client = tokio::spawn(client);
            tokio::select! {
                r = &mut client => r.unwrap(),
                _ = app.event_loop(&mut stream) => panic!("app exited"),
            }
        }
        // bridge:end

        // bridge:begin
        if matches!(variant, Variant::DiffGutter | Variant::InlineDiff) {
            // the diff worker computes the first hunks in the background
            for _ in 0..200 {
                let doc = helix_view::doc!(app.editor);
                if doc.diff_handle().is_some_and(|h| h.load().len() > 400) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        // bridge:end
        send(&tx, MOVE, WARMUP_PASSES);
        app.event_loop_until_idle(&mut stream).await;

        let keys = send(&tx, &keys, passes);
        let drawn = helix_term::bridge::views_drawn();
        let started = Instant::now();
        app.event_loop_until_idle(&mut stream).await;
        let elapsed = started.elapsed();
        if std::env::var_os("KEYPRESS_DEBUG").is_some() {
            let (view, doc) = helix_view::current_ref!(app.editor);
            let text = doc.text().slice(..);
            let cursor = doc.selection(view.id).primary().cursor(text);
            eprintln!(
                "KEYPRESS_DEBUG {variant:?} drawn={} cursor_line={} anchor_line={} mode={:?} status={:?} hunks={:?}",
                helix_term::bridge::views_drawn() - drawn,
                text.char_to_line(cursor),
                text.char_to_line(doc.view_offset(view.id).anchor),
                app.editor.mode(),
                app.editor.get_status(),
                doc.diff_handle().map(|h| h.load().len()),
            );
        }

        // bridge:begin
        if variant == Variant::Subscribed {
            assert!(
                received.load(std::sync::atomic::Ordering::Relaxed) > 0,
                "the client received events"
            );
        }
        // bridge:end
        finish(&mut app, &tx, &mut stream).await;
        (elapsed, keys)
    })
}

async fn finish(
    app: &mut Application,
    tx: &Keys,
    stream: &mut UnboundedReceiverStream<std::io::Result<Event>>,
) {
    send(tx, "<esc>:qa!<ret>", 1);
    let _ = tokio::time::timeout(Duration::from_secs(5), app.event_loop(stream)).await;
    let _ = app.close().await;
}

#[test]
#[ignore = "benchmark: run with bb-bridge/bench/keypress.sh"]
fn keypress_bench() {
    let file = tempfile::Builder::new().suffix(".go").tempfile().unwrap();
    std::fs::write(file.path(), go_source()).unwrap();
    let variants: &[Variant] = &[
        Variant::Off,
        // bridge:begin
        Variant::NoClient,
        Variant::Subscribed,
        Variant::Diagnostics,
        Variant::Annotations,
        Variant::DiffGutter,
        Variant::InlineDiff,
        // bridge:end
    ];
    let only = std::env::var("KEYPRESS_ONLY").unwrap_or_default();
    let variants: Vec<Variant> = variants
        .iter()
        .copied()
        .filter(|v| only.is_empty() || only.split(',').any(|o| o == format!("{v:?}")))
        .collect();
    let sequences = [
        ("move", MOVE, setting("KEYPRESS_PASSES", MOVE_PASSES)),
        ("edit", EDIT, setting("KEYPRESS_EDIT_PASSES", EDIT_PASSES)),
    ];
    for (name, keys, passes) in sequences {
        let mut times: Vec<Vec<f64>> = vec![Vec::new(); variants.len()];
        let mut count = 0;
        for _ in 0..setting("KEYPRESS_ROUNDS", ROUNDS) {
            // interleave variants so drift on a shared machine hits all of them
            for (i, variant) in variants.iter().enumerate() {
                let (elapsed, n) = run(*variant, file.path(), keys, passes);
                count = n;
                times[i].push(elapsed.as_secs_f64() * 1000.0);
            }
        }
        for (variant, mut t) in variants.iter().zip(times) {
            t.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = t[t.len() / 2];
            println!(
                "KEYPRESS seq={name} variant={variant:?} keys={count} median_ms={median:.1} min_ms={:.1} per_key_us={:.1} runs_ms={:?}",
                t[0],
                median * 1000.0 / count as f64,
                t.iter().map(|x| (x * 10.0).round() / 10.0).collect::<Vec<_>>()
            );
        }
    }
}
