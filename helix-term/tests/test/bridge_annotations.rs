//! bb bridge step 2d: annotations/set, annotations/clear, `:bb-annotations`,
//! editor/reveal. Drives a real `Application` over its socket and looks at
//! what the document holds; bb-bridge/bench/look.py captures how it looks.

use std::time::Duration;

use helix_view::annotations::bridge::{self as data, Kind};
use helix_view::{current, doc, view, Editor};
use serde_json::{json, Value};

use super::bridge::{Bridge, Client};
use super::*;

fn pos(line: u64, col: u64) -> Value {
    json!({ "line": line, "col": col })
}

fn canonical(path: &std::path::Path) -> String {
    helix_stdx::path::canonicalize(path)
        .to_string_lossy()
        .into_owned()
}

/// (first line, last line, text) of every annotation in the focused
/// document, 1-based like the protocol.
fn shown(editor: &Editor) -> Vec<(usize, usize, String)> {
    let doc = doc!(editor);
    doc.bridge_annotations
        .items()
        .iter()
        .map(|i| (i.first_line + 1, i.last_line + 1, i.text.clone()))
        .collect()
}

fn ten_lines() -> String {
    (1..=10).map(|i| format!("line {i}\n")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn annotations_set_clear_and_follow_edits() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents(ten_lines())?;
    let path = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;

    let p = path.clone();
    let hello = bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let hello = c.hello("t").await?;
            let set = c
                .call(
                    "annotations/set",
                    json!({ "namespace": "review", "path": p, "items": [
                        { "line": 3, "endLine": 4, "text": "first\tcomment\r\nsecond line" },
                        { "line": 8, "text": "a note", "kind": "note", "sign": "●", "highlight": true },
                        { "line": 3, "text": "the reply", "kind": "reply", "severity": "warning" },
                    ]}),
                )
                .await?;
            assert_eq!(set["count"], 3);
            // a second namespace stacks after the first on the same line
            let set = c
                .call(
                    "annotations/set",
                    json!({ "namespace": "agent", "path": p, "items": [
                        { "line": 4, "text": "from the agent" },
                    ]}),
                )
                .await?;
            assert_eq!(set["count"], 1);
            Ok(hello)
        })
        .await?;
    let caps = hello["capabilities"].as_array().unwrap();
    assert!(caps.contains(&json!("annotations")));
    assert!(caps.contains(&json!("reveal")));
    assert_eq!(
        shown(&bridge.app.editor),
        [
            (3, 3, "the reply".into()),
            (3, 4, "first    comment\nsecond line".into()),
            (4, 4, "from the agent".into()),
            (8, 8, "a note".into()),
        ]
    );
    {
        let doc = doc!(bridge.app.editor);
        let items = doc.bridge_annotations.items();
        assert_eq!(items[0].kind, Kind::Reply);
        assert_eq!(items[3].kind, Kind::Note);
        assert_eq!(items[3].sign.as_deref(), Some("●"));
        assert!(items[3].highlight);
    }

    // A line opened above moves everything down; undo moves it back; text
    // typed inside a range keeps it.
    bridge
        .run(|keys, _| async move {
            keys.send("ggOabove<esc>")?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await?;
    let lines = |e: &Editor| {
        shown(e)
            .into_iter()
            .map(|(a, b, _)| (a, b))
            .collect::<Vec<_>>()
    };
    assert_eq!(lines(&bridge.app.editor), [(4, 4), (4, 5), (5, 5), (9, 9)]);
    bridge
        .run(|keys, _| async move {
            keys.send("u")?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await?;
    assert_eq!(lines(&bridge.app.editor), [(3, 3), (3, 4), (4, 4), (8, 8)]);

    // Deleting lines 3 and 4 (line endings included) removes what was on
    // them; the note below moves up.
    bridge
        .run(|keys, _| async move {
            keys.send("3ggxxd")?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await?;
    assert_eq!(lines(&bridge.app.editor), [(6, 6)]);
    // Undo brings the lines back but not the annotations (like diagnostics,
    // until the client sets them again).
    bridge
        .run(|keys, _| async move {
            keys.send("u")?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await?;
    assert_eq!(lines(&bridge.app.editor), [(8, 8)]);

    // Reload from disk maps them through the diff.
    let mut new_text = String::from("inserted\n");
    new_text.push_str(&ten_lines());
    std::fs::write(file.path(), new_text)?;
    bridge
        .run(|keys, _| async move {
            keys.send(":reload<ret>")?;
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok(())
        })
        .await?;
    assert_eq!(lines(&bridge.app.editor), [(9, 9)]);

    let p = path.clone();
    bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            // empty items clear one namespace in that file
            let set = c
                .call(
                    "annotations/set",
                    json!({ "namespace": "review", "path": p, "items": [] }),
                )
                .await?;
            assert_eq!(set["count"], 0);
            c.call(
                "annotations/set",
                json!({ "namespace": "agent", "path": p, "items": [{ "line": 1, "text": "x" }] }),
            )
            .await?;
            c.call("annotations/clear", json!({ "namespace": "agent" }))
                .await?;

            // errors
            let err = c
                .error(
                    "annotations/set",
                    json!({ "namespace": "", "path": p, "items": [] }),
                )
                .await?;
            assert_eq!(err["data"]["field"], "namespace");
            let err = c
                .error(
                    "annotations/set",
                    json!({ "namespace": "n", "path": p, "items": [{ "line": 1, "text": "x", "sign": "ab" }] }),
                )
                .await?;
            assert_eq!(err["data"]["field"], "sign");
            let err = c
                .error(
                    "annotations/set",
                    json!({ "namespace": "n", "path": p, "items": [{ "line": 1, "text": "x".repeat(2001) }] }),
                )
                .await?;
            assert_eq!(err["code"], -32602);
            let err = c
                .error(
                    "annotations/set",
                    json!({ "namespace": "n", "path": p, "items": [{ "line": 1, "text": "x", "kind": "shout" }] }),
                )
                .await?;
            assert_eq!(err["code"], -32602);
            let err = c
                .error(
                    "annotations/set",
                    json!({ "namespace": "n", "path": format!("{p}.missing"), "items": [] }),
                )
                .await?;
            assert_eq!(err["code"], 1001);
            Ok(())
        })
        .await?;
    assert!(shown(&bridge.app.editor).is_empty());
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn annotations_for_a_file_not_open_wait_for_it() -> anyhow::Result<()> {
    let open = helpers::temp_file_with_contents("open\n")?;
    let later = helpers::temp_file_with_contents(ten_lines())?;
    let later_path = canonical(later.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(open.path(), None))?;
    let p = later_path.clone();
    bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let set = c
                .call(
                    "annotations/set",
                    json!({ "namespace": "review", "path": p, "items": [
                        { "line": 2, "text": "waiting" },
                        { "line": 99, "text": "past the end goes to the last line" },
                    ]}),
                )
                .await?;
            assert_eq!(set["count"], 2);
            // a namespace cleared before the file opens shows nothing
            c.call(
                "annotations/set",
                json!({ "namespace": "gone", "path": p, "items": [{ "line": 1, "text": "x" }] }),
            )
            .await?;
            c.call(
                "annotations/clear",
                json!({ "namespace": "gone", "path": p }),
            )
            .await?;
            c.call("editor/open", json!({ "path": p })).await?;
            Ok(())
        })
        .await?;
    assert_eq!(
        shown(&bridge.app.editor),
        [
            (2, 2, "waiting".into()),
            (11, 11, "past the end goes to the last line".into()),
        ]
    );
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn toggle_command() -> anyhow::Result<()> {
    let mut bridge = Bridge::start(AppBuilder::new())?;
    assert!(data::shown());
    bridge
        .run(|keys, _| async move {
            keys.send(":bb-annotations<ret>")?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await?;
    let hidden = !data::shown();
    let status = bridge.app.editor.get_status().map(|(s, _)| s.to_string());
    bridge
        .run(|keys, _| async move {
            keys.send(":bb-annotations<ret>")?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await?;
    assert!(hidden);
    assert_eq!(status.as_deref(), Some("bb annotations hidden"));
    assert!(data::shown());
    bridge.finish().await?;
    Ok(())
}

fn reveal_state(editor: &Editor) -> Option<(usize, usize)> {
    let reveal = doc!(editor).bridge_annotations.reveal.as_ref()?;
    Some((reveal.overlays.len(), reveal.removed.len()))
}

#[tokio::test(flavor = "multi_thread")]
async fn reveal_animates_and_any_key_ends_it() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents(ten_lines())?;
    let path = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;

    let p = path.clone();
    let result = bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let result = c
                .call(
                    "editor/reveal",
                    json!({
                        "path": p,
                        "ranges": [{ "start": pos(2, 1), "end": pos(4, 1) }, { "start": pos(9, 1), "end": pos(9, 3) }],
                        "removed": [{ "line": 3, "text": "old line\nand another" }],
                        "durationMs": 3000,
                    }),
                )
                .await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(result)
        })
        .await?;
    assert_eq!(result, json!({ "animated": 2, "deferred": false }));
    let (covered, removed) = reveal_state(&bridge.app.editor).expect("animating");
    // "line 2", "line 3", "li": 14 graphemes that are not whitespace, a few
    // already uncovered
    assert!(covered > 0 && covered <= 14, "{covered}");
    assert_eq!(removed, 2);

    bridge
        .run(|keys, _| async move {
            keys.send("j")?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(())
        })
        .await?;
    assert!(reveal_state(&bridge.app.editor).is_none(), "a key ends it");

    // A short one ends by itself; an edit ends a long one.
    let p = path.clone();
    bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let r = c
                .call(
                    "editor/reveal",
                    json!({ "path": p, "ranges": [{ "start": pos(1, 1), "end": pos(3, 1) }], "durationMs": 50 }),
                )
                .await?;
            assert_eq!(r["animated"], 1);
            tokio::time::sleep(Duration::from_millis(600)).await;
            let r = c
                .call(
                    "editor/reveal",
                    json!({ "path": p, "ranges": [{ "start": pos(1, 1), "end": pos(3, 1) }], "durationMs": 3000 }),
                )
                .await?;
            assert_eq!(r["animated"], 1);
            Ok(())
        })
        .await?;
    assert!(reveal_state(&bridge.app.editor).is_some());
    {
        // an edit to the document ends its animation
        let (view, doc) = current!(bridge.app.editor);
        let tx = helix_core::Transaction::insert(
            doc.text(),
            &helix_core::Selection::point(0),
            "x".into(),
        );
        doc.apply(&tx, view.id);
        assert!(doc.bridge_annotations.reveal.is_none());
    }

    // errors and the off switch
    let p = path.clone();
    bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let err = c
                .error(
                    "editor/reveal",
                    json!({ "path": p, "ranges": [], "durationMs": 10 }),
                )
                .await?;
            assert_eq!(err["data"]["field"], "durationMs");
            let err = c
                .error(
                    "editor/reveal",
                    json!({ "path": p, "ranges": [], "interrupt": "never" }),
                )
                .await?;
            assert_eq!(err["data"]["field"], "interrupt");
            let err = c
                .error(
                    "editor/reveal",
                    json!({ "path": format!("{p}.missing"), "ranges": [] }),
                )
                .await?;
            assert_eq!(err["code"], 1001);
            // nothing to show
            let r = c
                .call("editor/reveal", json!({ "path": p, "ranges": [] }))
                .await?;
            assert_eq!(r["animated"], 0);
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reveal_off_switch() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents(ten_lines())?;
    let path = canonical(file.path());
    let mut config = helpers::test_config();
    config.editor.bridge.reveal = false;
    let mut bridge = Bridge::start(
        AppBuilder::new()
            .with_config(config)
            .with_file(file.path(), None),
    )?;
    bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let r = c
                .call(
                    "editor/reveal",
                    json!({ "path": path, "ranges": [{ "start": pos(1, 1), "end": pos(2, 1) }] }),
                )
                .await?;
            assert_eq!(r, json!({ "animated": 0, "deferred": false }));
            Ok(())
        })
        .await?;
    assert!(reveal_state(&bridge.app.editor).is_none());
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reveal_follow_opens_and_moves() -> anyhow::Result<()> {
    let shown_file = helpers::temp_file_with_contents("a\n")?;
    let other = helpers::temp_file_with_contents(ten_lines())?;
    let other_path = canonical(other.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(shown_file.path(), None))?;
    let p = other_path.clone();
    let r = bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let r = c
                .call(
                    "editor/reveal",
                    json!({ "path": p, "ranges": [{ "start": pos(1, 1), "end": pos(2, 1) }] }),
                )
                .await?;
            assert_eq!(r["animated"], 0, "not open and no follow");
            c.call(
                "editor/reveal",
                json!({ "path": p, "ranges": [{ "start": pos(7, 1), "end": pos(8, 1) }], "follow": true, "durationMs": 3000 }),
            )
            .await
        })
        .await?;
    assert_eq!(r["animated"], 1);
    let doc = doc!(bridge.app.editor);
    assert_eq!(
        doc.path().map(|p| p.to_string_lossy().into_owned()),
        Some(other_path)
    );
    let text = doc.text().slice(..);
    let cursor = doc
        .selection(view!(bridge.app.editor).id)
        .primary()
        .cursor(text);
    assert_eq!(text.char_to_line(cursor), 6);
    assert!(doc.bridge_annotations.reveal.is_some());
    bridge.finish().await?;
    Ok(())
}
