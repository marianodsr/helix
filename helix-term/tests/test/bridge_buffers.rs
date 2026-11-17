//! bb bridge, step 2a: editor/reload, the focus check, locations and
//! editor/applyEdits, driven over the socket of a real `Application`.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::bail;
use serde_json::{json, Value};

use super::bridge::{Bridge, Client};
use super::*;

fn pos(line: u64, col: u64) -> Value {
    json!({ "line": line, "col": col })
}

fn range(sl: u64, sc: u64, el: u64, ec: u64) -> Value {
    json!({ "start": pos(sl, sc), "end": pos(el, ec) })
}

fn canonical(path: &Path) -> String {
    helix_stdx::path::canonicalize(path)
        .to_string_lossy()
        .into_owned()
}

/// Write `text` so the file's mtime is certain to differ from before.
fn rewrite(path: &str, text: &str) -> anyhow::Result<()> {
    std::thread::sleep(Duration::from_millis(5));
    std::fs::write(path, text)?;
    Ok(())
}

async fn state(c: &mut Client) -> anyhow::Result<Value> {
    c.call("editor/state", json!({})).await
}

async fn text(c: &mut Client, path: &str) -> anyhow::Result<String> {
    let t = c.call("editor/text", json!({ "path": path })).await?;
    Ok(t["text"].as_str().unwrap_or_default().to_owned())
}

/// Poll `editor/state` until `f` holds.
async fn until(
    c: &mut Client,
    what: &str,
    mut f: impl FnMut(&Value) -> bool,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let s = state(c).await?;
        if f(&s) {
            return Ok(s);
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}: {s}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Poll `editor/text` until the buffer holds `want`.
async fn until_text(c: &mut Client, path: &str, want: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let got = text(c, path).await?;
        if got == want {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("timed out waiting for {path} to hold {want:?}, it holds {got:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn doc<'a>(state: &'a Value, path: &str) -> Option<&'a Value> {
    state["documents"]
        .as_array()?
        .iter()
        .find(|d| d["path"] == path)
}

fn status(bridge: &Bridge) -> String {
    bridge
        .app
        .editor
        .get_status()
        .map(|(m, _)| m.to_string())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn capabilities_are_listed() -> anyhow::Result<()> {
    let mut bridge = Bridge::start(AppBuilder::new())?;
    let hello = bridge
        .run(|_, socket| async move { Client::connect(&socket).await?.hello("t").await })
        .await?;
    let caps = hello["capabilities"].as_array().unwrap();
    for cap in ["editor", "reload", "locations", "applyEdits"] {
        assert!(caps.contains(&json!(cap)), "{cap} in {caps:?}");
    }
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_reports_and_keeps_selections_in_every_view() -> anyhow::Result<()> {
    let a = helpers::temp_file_with_contents("l1\nl2\nl3\nl4\n")?;
    let b = helpers::temp_file_with_contents("b1\n")?;
    let c_file = helpers::temp_file_with_contents("c1\n")?;
    let d = helpers::temp_file_with_contents("d1\n")?;
    let (ap, bp, cp, dp) = (
        canonical(a.path()),
        canonical(b.path()),
        canonical(c_file.path()),
        canonical(d.path()),
    );
    // cursor on line 3 ("l3"), 0-based position
    let mut bridge = Bridge::start(
        AppBuilder::new().with_file(a.path(), Some(helix_core::Position::new(2, 0))),
    )?;
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["externalChange", "documents"]).await?;
            for p in [&bp, &cp, &dp] {
                c.call("editor/open", json!({ "path": p, "focus": false }))
                    .await?;
            }
            // b gets unsaved changes
            c.call(
                "editor/applyEdits",
                json!({ "path": bp, "edits": [{ "range": range(1, 1, 1, 1), "newText": "mine " }] }),
            )
            .await?;
            // two views of a: the second one's cursor goes to line 4
            keys.send(":vsplit<ret>j")?;
            until(&mut c, "the split", |s| {
                s["focused"]["selections"][0]["start"] == pos(4, 1)
            })
            .await?;

            rewrite(&ap, "new\nl1\nl2\nl3\nl4\n")?;
            rewrite(&bp, "theirs\n")?;
            std::fs::remove_file(&cp)?;
            c.drain(Duration::from_millis(150)).await?;

            let r = c.call("editor/reload", json!({})).await?;
            assert_eq!(r["reloaded"], json!([ap]));
            assert_eq!(r["skippedModified"], json!([bp]));
            assert_eq!(r["missing"], json!([cp]));
            assert_eq!(r["unchanged"], json!([dp]));
            assert!(r.get("deferred").is_none(), "no follow, no deferred");

            assert_eq!(text(&mut c, &ap).await?, "new\nl1\nl2\nl3\nl4\n");
            let ext = c.event("event/externalChange").await?;
            assert_eq!(ext, json!({ "path": bp, "modified": true }));
            // the reloaded buffer is reported clean
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let ev = c.event("event/documentChanged").await?;
                if ev["path"] == ap.as_str() && ev["modified"] == false {
                    break;
                }
                assert!(Instant::now() < deadline);
            }

            // both views' cursors moved down with their lines
            let s = state(&mut c).await?;
            assert_eq!(s["focused"]["selections"][0]["start"], pos(5, 1));
            assert_eq!(doc(&s, &ap).unwrap()["modified"], false);
            keys.send("<C-w>w")?;
            until(&mut c, "the other view", |s| {
                s["focused"]["selections"][0]["start"] == pos(4, 1)
            })
            .await?;

            // the same change is announced once
            let r = c.call("editor/reload", json!({})).await?;
            assert_eq!(r["skippedModified"], json!([bp]));
            assert_eq!(r["unchanged"].as_array().unwrap().len(), 2);
            let later = c.drain(Duration::from_millis(200)).await?;
            assert!(
                !later.iter().any(|e| e["method"] == "event/externalChange"),
                "{later:?}"
            );

            // `paths` limits the reload; `force` takes the disk version
            let r = c
                .call("editor/reload", json!({ "paths": [bp], "force": true }))
                .await?;
            assert_eq!(r["reloaded"], json!([bp]));
            assert_eq!(r["unchanged"], json!([]));
            assert_eq!(text(&mut c, &bp).await?, "theirs\n");

            // `u` undoes a reload
            keys.send("u")?;
            until_text(&mut c, &ap, "l1\nl2\nl3\nl4\n").await?;

            let err = c
                .error("editor/reload", json!({ "interrupt": "later" }))
                .await?;
            assert_eq!(err["code"], -32602);
            assert_eq!(err["data"]["field"], "interrupt");
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_follow_moves_to_the_first_change() -> anyhow::Result<()> {
    let a = helpers::temp_file_with_contents("one\n")?;
    let b = helpers::temp_file_with_contents("x1\nx2\nx3\nx4\nx5\n")?;
    let (ap, bp) = (canonical(a.path()), canonical(b.path()));
    let mut bridge = Bridge::start(AppBuilder::new().with_file(a.path(), None))?;
    let (ap2, bp2) = (ap.clone(), bp.clone());
    bridge
        .run(move |_, socket| async move {
            let (ap, bp) = (ap2, bp2);
            let mut c = Client::connect(&socket).await?;
            c.call("editor/open", json!({ "path": bp, "focus": false }))
                .await?;
            rewrite(&bp, "x1\nx2\nx3\nx4 changed é\nx5\n")?;
            let r = c
                .call(
                    "editor/reload",
                    json!({ "follow": true, "reveal": true, "interrupt": "always" }),
                )
                .await?;
            assert_eq!(r["reloaded"], json!([bp]));
            assert_eq!(r["deferred"], false);
            let s = state(&mut c).await?;
            assert_eq!(s["focused"]["path"], bp.as_str());
            // the diff is per character: the first insertion is " changed é"
            assert_eq!(s["focused"]["selections"][0]["start"], pos(4, 3));
            assert_ne!(s["focused"]["path"], ap.as_str());
            Ok(())
        })
        .await?;
    assert!(
        helix_view::doc!(bridge.app.editor)
            .bridge_annotations
            .reveal
            .is_some(),
        "the reload's reveal plays where the view followed"
    );

    // while the user is busy, follow and goto wait for :bb-go
    let (ap2, bp2) = (ap.clone(), bp.clone());
    bridge
        .run(move |keys, socket| async move {
            let (ap, bp) = (ap2, bp2);
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["mode"]).await?;
            c.call("editor/open", json!({ "path": ap, "interrupt": "always" }))
                .await?;
            keys.send("i")?;
            assert_eq!(c.event("event/mode").await?["mode"], "insert");
            rewrite(&bp, "x1\nx2 again\nx3\nx4 changed é\nx5\n")?;
            let r = c
                .call("editor/reload", json!({ "follow": true, "reveal": true }))
                .await?;
            assert_eq!(r["reloaded"], json!([bp]));
            assert_eq!(r["deferred"], true, "{r}");
            assert_eq!(state(&mut c).await?["focused"]["path"], ap.as_str());
            keys.send("<esc>")?;
            assert_eq!(c.event("event/mode").await?["mode"], "normal");
            keys.send(":bb-go<ret>")?;
            until(&mut c, "the deferred follow", |s| {
                s["focused"]["path"] == bp.as_str()
                    && s["focused"]["selections"][0]["start"] == pos(2, 3)
            })
            .await?;
            Ok(())
        })
        .await?;
    assert!(
        helix_view::doc!(bridge.app.editor)
            .bridge_annotations
            .reveal
            .is_some(),
        ":bb-go replays the reveal where it lands"
    );

    let (ap2, bp2) = (ap.clone(), bp.clone());
    bridge
        .run(move |keys, socket| async move {
            let (ap, bp) = (ap2, bp2);
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["mode"]).await?;
            keys.send("i")?;
            assert_eq!(c.event("event/mode").await?["mode"], "insert");
            let items = json!([{ "path": ap, "line": 1 }]);
            let r = c
                .call(
                    "locations/set",
                    json!({ "list": "tour", "items": items, "goto": 0 }),
                )
                .await?;
            assert_eq!(r, json!({ "count": 1, "deferred": true }));
            assert_eq!(state(&mut c).await?["focused"]["path"], bp.as_str());
            let r = c
                .call(
                    "locations/set",
                    json!({ "list": "tour", "items": items, "goto": 0, "interrupt": "always" }),
                )
                .await?;
            assert_eq!(r["deferred"], false);
            assert_eq!(state(&mut c).await?["focused"]["path"], ap.as_str());
            keys.send("<esc>")?;
            assert_eq!(c.event("event/mode").await?["mode"], "normal");
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn focus_gained_reloads_clean_buffers_and_warns_about_modified() -> anyhow::Result<()> {
    use termina::event::Event;
    let a = helpers::temp_file_with_contents("clean\n")?;
    let b = helpers::temp_file_with_contents("dirty\n")?;
    let (ap, bp) = (canonical(a.path()), canonical(b.path()));
    let mut bridge = Bridge::start(AppBuilder::new().with_file(a.path(), None))?;
    let (ap2, bp2) = (ap.clone(), bp.clone());
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["externalChange"]).await?;
            c.call("editor/open", json!({ "path": bp2, "focus": false }))
                .await?;
            c.call(
                "editor/applyEdits",
                json!({ "path": bp2, "edits": [{ "range": range(1, 1, 1, 1), "newText": "my " }] }),
            )
            .await?;
            // nothing changed: nothing happens
            keys.event(Event::FocusIn)?;
            let quiet = c.drain(Duration::from_millis(200)).await?;
            assert!(quiet.is_empty(), "{quiet:?}");

            rewrite(&ap2, "clean, changed\n")?;
            rewrite(&bp2, "dirty, changed\n")?;
            keys.event(Event::FocusIn)?;
            until_text(&mut c, &ap2, "clean, changed\n").await?;
            let ext = c.event("event/externalChange").await?;
            assert_eq!(ext["path"], bp2.as_str());
            assert_eq!(text(&mut c, &bp2).await?, "my dirty\n", "unsaved work kept");
            Ok(())
        })
        .await?;
    let warning = status(&bridge);
    assert!(warning.contains("changed on disk"), "{warning}");
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["externalChange"]).await?;
            // the same change is not announced again
            keys.event(Event::FocusIn)?;
            let quiet = c.drain(Duration::from_millis(300)).await?;
            assert!(quiet.is_empty(), "{quiet:?}");
            let s = state(&mut c).await?;
            assert_eq!(doc(&s, &ap).unwrap()["modified"], false);
            assert_eq!(doc(&s, &bp).unwrap()["modified"], true);
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn locations_walk_picker_and_notes() -> anyhow::Result<()> {
    let a = helpers::temp_file_with_contents("a1\na2\na3\na4\n")?;
    let b = helpers::temp_file_with_contents("b1\nb2\nb3\nb4\nb5\nb6\n")?;
    let (ap, bp) = (canonical(a.path()), canonical(b.path()));
    let gone = format!("{ap}.gone");
    let mut bridge = Bridge::start(AppBuilder::new().with_file(a.path(), None))?;
    let (ap2, bp2) = (ap.clone(), bp.clone());
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let items = json!([
                { "path": ap2, "line": 2, "note": "start here" },
                { "path": bp2, "line": 5, "col": 2, "endLine": 6, "label": "handler", "note": "then here" },
                { "path": gone, "line": 1, "note": "deleted" },
            ]);
            let r = c
                .call(
                    "locations/set",
                    json!({ "list": "tour", "title": "A tour", "items": items, "goto": 1 }),
                )
                .await?;
            assert_eq!(r, json!({ "count": 3, "deferred": false }));
            let s = state(&mut c).await?;
            assert_eq!(s["focused"]["path"], bp2.as_str());
            // col 2 of line 5 through line 6, its line ending included
            // (as `editor/open` with `endLine` selects)
            assert_eq!(s["focused"]["selections"][0], range(5, 2, 7, 1));

            let err = c
                .error("locations/set", json!({ "list": "tour", "items": items, "goto": 3 }))
                .await?;
            assert_eq!(err["data"]["field"], "goto");
            let many: Vec<Value> = (0..2001)
                .map(|i| json!({ "path": "x", "line": i + 1 }))
                .collect();
            let err = c
                .error("locations/set", json!({ "list": "big", "items": many }))
                .await?;
            assert_eq!(err["data"]["field"], "items");
            Ok(())
        })
        .await?;
    assert_eq!(status(&bridge), "tour 2/3: then here");

    // the walk skips the missing file and wraps around
    let (ap2, bp2) = (ap.clone(), bp.clone());
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            keys.send(":bb-next<ret>")?;
            until(&mut c, "bb-next", |s| s["focused"]["path"] == ap2.as_str()).await?;
            let s = state(&mut c).await?;
            assert_eq!(s["focused"]["selections"][0]["start"], pos(2, 1));
            Ok(())
        })
        .await?;
    assert_eq!(
        status(&bridge),
        "tour 1/3: start here (skipped 1 missing file, wrapped)"
    );
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            keys.send(":bb-prev<ret>")?;
            until(&mut c, "bb-prev", |s| s["focused"]["path"] == bp2.as_str()).await?;
            Ok(())
        })
        .await?;
    assert_eq!(
        status(&bridge),
        "tour 2/3: then here (skipped 1 missing file, wrapped)"
    );

    // standing on an item keeps its note on the status line
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            keys.send("<esc>gg")?;
            until(&mut c, "line 1", |s| {
                s["focused"]["selections"][0]["start"] == pos(1, 1)
            })
            .await?;
            keys.send("5gg")?;
            until(&mut c, "line 5", |s| {
                s["focused"]["selections"][0]["start"]["line"] == 5
            })
            .await?;
            Ok(())
        })
        .await?;
    assert_eq!(status(&bridge), "tour 2/3: then here");
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            keys.send("gg")?;
            until(&mut c, "line 1", |s| {
                s["focused"]["selections"][0]["start"] == pos(1, 1)
            })
            .await?;
            Ok(())
        })
        .await?;
    assert_eq!(status(&bridge), "", "off the item, no note");

    // the picker lists the items in order and jumps to the chosen one
    let ap2 = ap.clone();
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            keys.send(":bb-locations tour<ret>")?;
            super::bridge::until_overlay("picker").await?;
            keys.send("<ret>")?;
            until(&mut c, "the picker's jump", |s| {
                s["focused"]["path"] == ap2.as_str()
            })
            .await?;

            c.call("locations/clear", json!({ "list": "tour" })).await?;
            Ok(())
        })
        .await?;
    bridge
        .run(move |keys, socket| async move {
            let _c = Client::connect(&socket).await?;
            keys.send(":bb-next<ret>")?;
            super::bridge::until_status("'bb-next': No active bb locations list").await
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_edits_one_undo_step_versions_and_errors() -> anyhow::Result<()> {
    let a = helpers::temp_file_with_contents("aé🦀b\r\nline two\r\nend")?;
    let b = helpers::temp_file_with_contents("background\n")?;
    let (ap, bp) = (canonical(a.path()), canonical(b.path()));
    let missing = format!("{ap}.missing");
    let mut bridge = Bridge::start(AppBuilder::new().with_file(a.path(), None))?;
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let before = state(&mut c).await?["focused"]["version"].as_i64().unwrap();
            let r = c
                .call(
                    "editor/applyEdits",
                    json!({
                        "path": ap,
                        "version": before,
                        "edits": [
                            // given out of order, positions in the old text
                            { "range": range(3, 4, 3, 4), "newText": "\r\nnew last" },
                            { "range": range(1, 3, 1, 4), "newText": "CRAB" },
                            { "range": range(2, 1, 2, 5), "newText": "LINE" },
                        ],
                    }),
                )
                .await?;
            let after = r["version"].as_i64().unwrap();
            assert!(after > before);
            assert_eq!(r["modified"], true);
            assert_eq!(r["path"], ap.as_str());
            let edited = "aéCRABb\r\nLINE two\r\nend\r\nnew last";
            assert_eq!(text(&mut c, &ap).await?, edited);

            // stale version
            let err = c
                .error(
                    "editor/applyEdits",
                    json!({ "path": ap, "version": before, "edits": [] }),
                )
                .await?;
            assert_eq!(err["code"], 1002);
            assert_eq!(err["data"]["version"], after);
            // overlap
            let err = c
                .error(
                    "editor/applyEdits",
                    json!({ "path": ap, "edits": [
                        { "range": range(1, 1, 1, 4), "newText": "x" },
                        { "range": range(1, 2, 1, 3), "newText": "y" },
                    ] }),
                )
                .await?;
            assert_eq!(err["code"], -32602);
            assert_eq!(err["data"]["field"], "edits");
            assert_eq!(text(&mut c, &ap).await?, edited, "refused edits change nothing");
            let err = c
                .error("editor/applyEdits", json!({ "path": missing, "edits": [] }))
                .await?;
            assert_eq!(err["code"], 1001);

            // one `u` restores the buffer exactly, `U` redoes it
            keys.send("u")?;
            until_text(&mut c, &ap, "aé🦀b\r\nline two\r\nend").await?;
            keys.send("U")?;
            until_text(&mut c, &ap, edited).await?;

            // typing in insert mode and an edit never share an undo step
            keys.send("ggixy")?;
            until_text(&mut c, &ap, &format!("xy{edited}")).await?;
            c.call(
                "editor/applyEdits",
                json!({ "path": ap, "edits": [{ "range": range(2, 1, 2, 5), "newText": "L" }] }),
            )
            .await?;
            keys.send("z<esc>")?;
            let both = "xyzaéCRABb\r\nL two\r\nend\r\nnew last";
            until_text(&mut c, &ap, both).await?;
            keys.send("u")?;
            until_text(&mut c, &ap, "xyaéCRABb\r\nL two\r\nend\r\nnew last").await?;
            keys.send("u")?;
            until_text(&mut c, &ap, &format!("xy{edited}")).await?;

            // a file that is not open is opened in the background
            let r = c
                .call(
                    "editor/applyEdits",
                    json!({ "path": bp, "edits": [{ "range": range(1, 1, 1, 11), "newText": "fore" }] }),
                )
                .await?;
            assert_eq!(r["modified"], true);
            let s = state(&mut c).await?;
            assert_eq!(s["focused"]["path"], ap.as_str());
            assert_eq!(doc(&s, &bp).unwrap()["modified"], true);
            assert_eq!(std::fs::read_to_string(&bp)?, "background\n");

            // save writes it and reports it clean
            let r = c
                .call(
                    "editor/applyEdits",
                    json!({ "path": bp, "edits": [{ "range": range(1, 5, 1, 5), "newText": "ground" }], "save": true }),
                )
                .await?;
            assert_eq!(r["modified"], false);
            assert_eq!(std::fs::read_to_string(&bp)?, "foreground\n");

            // saving over a file changed on disk is refused before editing
            rewrite(&bp, "someone else\n")?;
            let err = c
                .error(
                    "editor/applyEdits",
                    json!({ "path": bp, "edits": [{ "range": range(1, 1, 1, 1), "newText": "x" }], "save": true }),
                )
                .await?;
            assert_eq!(err["code"], 1007);
            assert_eq!(text(&mut c, &bp).await?, "foreground\n");
            assert_eq!(std::fs::read_to_string(&bp)?, "someone else\n");
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_edits_maps_selections_in_other_views() -> anyhow::Result<()> {
    let a = helpers::temp_file_with_contents("one\ntwo\nthree\n")?;
    let ap = canonical(a.path());
    let mut bridge = Bridge::start(
        AppBuilder::new().with_file(a.path(), Some(helix_core::Position::new(2, 0))),
    )?;
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            // the new view's cursor goes to line 2, the first one stays on 3
            keys.send(":vsplit<ret>k")?;
            until(&mut c, "two views", |s| {
                s["focused"]["selections"][0]["start"] == pos(2, 1)
            })
            .await?;
            c.call(
                "editor/applyEdits",
                json!({ "path": ap, "edits": [{ "range": range(1, 1, 1, 1), "newText": "zero\n" }] }),
            )
            .await?;
            until(&mut c, "the focused cursor to move down", |s| {
                s["focused"]["selections"][0]["start"] == pos(3, 1)
            })
            .await?;
            keys.send("<C-w>w")?;
            until(&mut c, "the other cursor to move down", |s| {
                s["focused"]["selections"][0]["start"] == pos(4, 1)
            })
            .await?;
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}
