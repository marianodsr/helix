//! bb bridge, step 2b: diagnostics/get, event/diagnostics, lsp/servers and
//! lsp/query over the socket. Language servers are off in the test
//! harness, so diagnostics are injected the way Helix stores a server's
//! `publishDiagnostics`; queries against a real gopls are in
//! `bb-bridge/bench/live_lsp.py`.

use std::time::{Duration, Instant};

use helix_core::diagnostic::{Diagnostic, DiagnosticProvider, LanguageServerId, Severity};
use helix_core::Uri;
use helix_lsp::lsp;
use helix_view::events::DiagnosticsDidChange;
use helix_view::Editor;
use serde_json::{json, Value};
use slotmap::KeyData;

use super::bridge::{Bridge, Client};
use super::*;

fn canonical(path: &std::path::Path) -> String {
    helix_stdx::path::canonicalize(path)
        .to_string_lossy()
        .into_owned()
}

/// A server id no running server has: diagnostics from it are converted as
/// UTF-16, the LSP default.
fn provider() -> DiagnosticProvider {
    DiagnosticProvider::Lsp {
        server_id: LanguageServerId::from(KeyData::from_ffi((1 << 32) | 77)),
        identifier: None,
    }
}

fn diag(
    line: u32,
    start: u32,
    end: u32,
    severity: lsp::DiagnosticSeverity,
    message: &str,
) -> lsp::Diagnostic {
    lsp::Diagnostic {
        range: lsp::Range::new(
            lsp::Position::new(line, start),
            lsp::Position::new(line, end),
        ),
        severity: Some(severity),
        code: Some(lsp::NumberOrString::String("E1".into())),
        source: Some("test".into()),
        message: message.into(),
        ..Default::default()
    }
}

fn publish(editor: &mut Editor, path: &str, diags: Vec<lsp::Diagnostic>) {
    let uri = Uri::from(std::path::PathBuf::from(path));
    helix_term::bridge::diagnostics_published(&uri);
    editor.handle_lsp_diagnostics(&provider(), uri, None, diags);
}

/// Diagnostics of an open buffer as Helix holds them (char offsets), and
/// the event Helix dispatches after storing them.
fn set_doc_diagnostics(editor: &mut Editor, path: &str, diags: &[(usize, usize, Severity, &str)]) {
    let id = editor
        .document_id_by_path(std::path::Path::new(path))
        .expect("open");
    let doc = editor.document_mut(id).unwrap();
    let text = doc.text().clone();
    let diags = diags
        .iter()
        .map(|(start, end, severity, message)| Diagnostic {
            range: helix_core::diagnostic::Range {
                start: *start,
                end: *end,
            },
            ends_at_word: false,
            starts_at_word: false,
            zero_width: false,
            line: text.char_to_line(*start),
            message: (*message).into(),
            severity: Some(*severity),
            code: Some(helix_core::diagnostic::NumberOrString::String("E1".into())),
            provider: provider(),
            tags: Vec::new(),
            source: Some("test".into()),
            data: None,
        });
    doc.replace_diagnostics(diags, &[], None);
    helix_event::dispatch(DiagnosticsDidChange { editor, doc: id });
}

fn pos(line: u64, col: u64) -> Value {
    json!({ "line": line, "col": col })
}

#[tokio::test(flavor = "multi_thread")]
async fn capabilities_and_no_language_server() -> anyhow::Result<()> {
    let file = tempfile::Builder::new().suffix(".go").tempfile()?;
    std::fs::write(file.path(), "package main\n\nfunc main() {}\n")?;
    let path = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    let p = path.clone();
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let hello = c.hello("t").await?;
            let caps = hello["capabilities"].as_array().unwrap();
            assert!(caps.contains(&json!("diagnostics")), "{hello}");
            assert!(caps.contains(&json!("lsp")), "{hello}");

            assert_eq!(c.call("lsp/servers", json!({})).await?, json!({ "servers": [] }));
            for kind in [
                "definition",
                "typeDefinition",
                "implementation",
                "references",
                "hover",
                "incomingCalls",
                "outgoingCalls",
            ] {
                let err = c
                    .error("lsp/query", json!({ "kind": kind, "path": p, "position": pos(3, 6) }))
                    .await?;
                assert_eq!(err["code"], 1004, "{kind}: {err}");
                assert_eq!(err["data"]["language"], "go");
                assert_eq!(err["data"]["kind"], kind);
            }
            let err = c
                .error("lsp/query", json!({ "kind": "documentSymbols", "path": p }))
                .await?;
            assert_eq!(err["code"], 1004);
            let err = c
                .error(
                    "lsp/query",
                    json!({ "kind": "renamePreview", "path": p, "position": pos(3, 6), "newName": "x" }),
                )
                .await?;
            assert_eq!(err["code"], 1004);
            let err = c
                .error("lsp/query", json!({ "kind": "workspaceSymbols", "query": "main" }))
                .await?;
            assert_eq!(err["code"], 1004);
            assert_eq!(err["data"]["language"], Value::Null);
            // a closed file, with and without ensure
            let other = p.replace(".go", "-other.go");
            std::fs::write(&other, "package main\n")?;
            for ensure in [true, false] {
                let err = c
                    .error(
                        "lsp/query",
                        json!({ "kind": "hover", "path": other, "position": pos(1, 1), "ensure": ensure }),
                    )
                    .await?;
                assert_eq!(err["code"], 1004, "ensure {ensure}: {err}");
                assert_eq!(err["data"]["language"], "go");
            }
            // ensure loaded and closed it again
            let state = c.call("editor/state", json!({})).await?;
            assert_eq!(state["documents"].as_array().unwrap().len(), 1, "{state}");
            std::fs::remove_file(&other)?;

            // params
            let err = c.error("lsp/query", json!({ "path": p })).await?;
            assert_eq!(err["data"]["field"], "kind");
            let err = c
                .error("lsp/query", json!({ "kind": "jump", "path": p }))
                .await?;
            assert_eq!(err["data"]["field"], "kind");
            let err = c
                .error("lsp/query", json!({ "kind": "hover", "position": pos(1, 1) }))
                .await?;
            assert_eq!(err["data"]["field"], "path");
            let err = c.error("lsp/query", json!({ "kind": "hover", "path": p })).await?;
            assert_eq!(err["data"]["field"], "position");
            let err = c
                .error("lsp/query", json!({ "kind": "renamePreview", "path": p, "position": pos(1, 1) }))
                .await?;
            assert_eq!(err["data"]["field"], "newName");
            let err = c
                .error(
                    "lsp/query",
                    json!({ "kind": "hover", "path": format!("{p}.missing"), "position": pos(1, 1) }),
                )
                .await?;
            assert_eq!(err["code"], 1001);
            let dir = std::path::Path::new(&p).parent().unwrap().to_string_lossy().into_owned();
            let err = c
                .error("lsp/query", json!({ "kind": "hover", "path": dir, "position": pos(1, 1) }))
                .await?;
            assert_eq!(err["code"], 1001);
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_open_and_closed_files() -> anyhow::Result<()> {
    // open buffer: positions in chars, from Helix's own diagnostics
    let open = tempfile::Builder::new().suffix(".txt").tempfile()?;
    std::fs::write(open.path(), "aé🦀x = 1\nline two\n")?;
    // closed file: the server's UTF-16 positions, converted against disk
    let closed = tempfile::Builder::new().suffix(".txt").tempfile()?;
    std::fs::write(closed.path(), "🦀🦀 bad\nok\n")?;
    let (open_path, closed_path) = (canonical(open.path()), canonical(closed.path()));
    let mut bridge = Bridge::start(AppBuilder::new().with_file(open.path(), None))?;
    let editor = &mut bridge.app.editor;
    // 'x' is char 3; line two starts at char 9
    set_doc_diagnostics(
        editor,
        &open_path,
        &[
            (3, 4, Severity::Warning, "x is unused"),
            (9, 13, Severity::Error, "bad line"),
            (14, 17, Severity::Hint, "hint"),
        ],
    );
    // "bad" starts at UTF-16 unit 5 = char 4
    publish(
        editor,
        &closed_path,
        vec![
            diag(0, 5, 8, lsp::DiagnosticSeverity::ERROR, "bad word"),
            diag(1, 0, 2, lsp::DiagnosticSeverity::INFORMATION, "fine"),
        ],
    );
    let (op, cp) = (open_path.clone(), closed_path.clone());
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let all = c.call("diagnostics/get", json!({})).await?;
            assert_eq!(all["total"], 5, "{all}");
            assert_eq!(
                all["counts"],
                json!({ "errors": 2, "warnings": 1, "infos": 1, "hints": 1 })
            );
            assert_eq!(all["truncated"], false);
            let items = all["items"].as_array().unwrap();
            // errors first, then by path and position
            let order: Vec<(&str, &str)> = items
                .iter()
                .map(|i| {
                    (
                        i["severity"].as_str().unwrap(),
                        i["message"].as_str().unwrap(),
                    )
                })
                .collect();
            let mut errors: Vec<(&str, &str)> = vec![("error", "bad line"), ("error", "bad word")];
            if cp < op {
                errors.reverse();
            }
            assert_eq!(&order[..2], &errors[..]);
            assert_eq!(
                &order[2..],
                &[
                    ("warning", "x is unused"),
                    ("info", "fine"),
                    ("hint", "hint")
                ]
            );

            let open = c.call("diagnostics/get", json!({ "path": op })).await?;
            assert_eq!(open["total"], 3);
            let x = &open["items"][1];
            assert_eq!(x["message"], "x is unused");
            assert_eq!(x["range"], json!({ "start": pos(1, 4), "end": pos(1, 5) }));
            assert_eq!(x["path"], op.as_str());
            assert_eq!(x["source"], "test");
            assert_eq!(x["code"], "E1");
            // the id belongs to no running server
            assert!(x.get("server").is_none());

            let closed = c.call("diagnostics/get", json!({ "path": cp })).await?;
            assert_eq!(
                closed["items"][0]["range"],
                json!({ "start": pos(1, 4), "end": pos(1, 7) })
            );
            assert_eq!(
                closed["counts"],
                json!({ "errors": 1, "warnings": 0, "infos": 1, "hints": 0 })
            );

            let warn = c
                .call("diagnostics/get", json!({ "minSeverity": "warning" }))
                .await?;
            assert_eq!(warn["total"], 3);
            assert_eq!(warn["counts"]["hints"], 1, "counts ignore minSeverity");
            let page = c
                .call("diagnostics/get", json!({ "limit": 2, "offset": 1 }))
                .await?;
            assert_eq!(page["items"].as_array().unwrap().len(), 2);
            assert_eq!(page["items"][0], items[1]);
            assert_eq!(page["truncated"], true);
            let only = c.call("diagnostics/get", json!({ "limit": 0 })).await?;
            assert_eq!(only, json!({ "counts": all["counts"], "total": 5 }));

            // a file that does not exist: not_found
            let err = c
                .error("diagnostics/get", json!({ "path": format!("{op}.none") }))
                .await?;
            assert_eq!(err["code"], 1001);
            let err = c
                .error("diagnostics/get", json!({ "minSeverity": "fatal" }))
                .await?;
            assert_eq!(err["code"], -32602);
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_events_are_coalesced_per_path() -> anyhow::Result<()> {
    let open = tempfile::Builder::new().suffix(".txt").tempfile()?;
    std::fs::write(open.path(), "one\ntwo\n")?;
    let closed = tempfile::Builder::new().suffix(".txt").tempfile()?;
    std::fs::write(closed.path(), "three\n")?;
    let (open_path, closed_path) = (canonical(open.path()), canonical(closed.path()));
    let mut bridge = Bridge::start(AppBuilder::new().with_file(open.path(), None))?;
    let (op, cp) = (open_path.clone(), closed_path.clone());
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["diagnostics"]).await?;
            // a burst: many publishes for two paths inside one window
            let (o, cl) = (op.clone(), cp.clone());
            for n in 1..=5usize {
                let (o, cl) = (o.clone(), cl.clone());
                helix_term::job::dispatch(move |editor, _| {
                    let errors: Vec<_> = (0..n).map(|i| (i, i + 1, Severity::Error, "e")).collect();
                    set_doc_diagnostics(editor, &o, &errors);
                    publish(
                        editor,
                        &cl,
                        vec![diag(0, 0, 1, lsp::DiagnosticSeverity::WARNING, "w")],
                    );
                })
                .await;
            }
            let events = c.drain(Duration::from_millis(600)).await?;
            let diag_events: Vec<&Value> = events
                .iter()
                .filter(|e| e["method"] == "event/diagnostics")
                .map(|e| &e["params"])
                .collect();
            let for_open: Vec<_> = diag_events
                .iter()
                .filter(|e| e["path"] == op.as_str())
                .collect();
            let for_closed: Vec<_> = diag_events
                .iter()
                .filter(|e| e["path"] == cp.as_str())
                .collect();
            // leading edge, then at most one more per window with the final state
            assert!((1..=2).contains(&for_open.len()), "{diag_events:?}");
            assert!((1..=2).contains(&for_closed.len()), "{diag_events:?}");
            assert_eq!(for_open.last().unwrap()["errors"], 5, "final state");
            assert_eq!(for_closed.last().unwrap()["warnings"], 1);
            assert_eq!(for_closed.last().unwrap()["errors"], 0);

            // cleared: an event with zero counts, at once after a quiet window
            let o = op.clone();
            let start = Instant::now();
            helix_term::job::dispatch(move |editor, _| set_doc_diagnostics(editor, &o, &[])).await;
            let ev = c.event("event/diagnostics").await?;
            assert_eq!(ev["path"], op.as_str());
            assert_eq!(ev["errors"], 0);
            assert!(
                start.elapsed() < Duration::from_millis(400),
                "{:?}",
                start.elapsed()
            );

            // unsubscribed: nothing
            c.call("events/unsubscribe", json!({ "events": ["diagnostics"] }))
                .await?;
            let o = op.clone();
            helix_term::job::dispatch(move |editor, _| {
                set_doc_diagnostics(editor, &o, &[(0, 1, Severity::Error, "e")])
            })
            .await;
            let events = c.drain(Duration::from_millis(400)).await?;
            assert!(
                events.iter().all(|e| e["method"] != "event/diagnostics"),
                "{events:?}"
            );
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

/// A few thousand diagnostics across many files that are not open.
#[tokio::test(flavor = "multi_thread")]
async fn diagnostics_many() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut bridge = Bridge::start(AppBuilder::new())?;
    let editor = &mut bridge.app.editor;
    let files = 100;
    let per_file = 48;
    for f in 0..files {
        let path = dir.path().join(format!("f{f:03}.go"));
        let text: String = (0..per_file)
            .map(|i| format!("var v{i} int = \"é{i}\"\n"))
            .collect();
        std::fs::write(&path, text)?;
        let severity = |i: u32| match i % 4 {
            0 => lsp::DiagnosticSeverity::ERROR,
            1 => lsp::DiagnosticSeverity::WARNING,
            2 => lsp::DiagnosticSeverity::INFORMATION,
            _ => lsp::DiagnosticSeverity::HINT,
        };
        let diags = (0..per_file)
            .map(|i| diag(i, 15, 18, severity(i), "cannot use \"é\" as int"))
            .collect();
        publish(editor, &canonical(&path), diags);
    }
    let total = files * per_file;
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            for (params, expect) in [
                (json!({ "limit": 0 }), 0),
                (json!({}), 100),
                (json!({ "limit": 500 }), 500),
                (json!({ "limit": 500, "offset": total - 10 }), 10),
                (json!({ "minSeverity": "error", "limit": 500 }), 500),
            ] {
                let mut times = Vec::new();
                let mut last = Value::Null;
                for _ in 0..20 {
                    let t = Instant::now();
                    last = c.call("diagnostics/get", params.clone()).await?;
                    times.push(t.elapsed());
                }
                times.sort();
                eprintln!(
                    "diagnostics/get {params} over {total}: p50 {:?} max {:?}",
                    times[times.len() / 2],
                    times[times.len() - 1]
                );
                assert_eq!(last["counts"]["errors"], total / 4);
                assert_eq!(
                    last.get("items").map_or(0, |i| i.as_array().unwrap().len()),
                    expect
                );
            }
            let first = c.call("diagnostics/get", json!({ "limit": 1 })).await?;
            assert_eq!(first["items"][0]["severity"], "error");
            assert_eq!(first["items"][0]["range"]["start"], pos(1, 16));
            assert!(first["items"][0]["path"]
                .as_str()
                .unwrap()
                .ends_with("f000.go"));
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}
