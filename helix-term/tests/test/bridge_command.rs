//! bb bridge, step 2c: panes, selections, `editor/command` and its policy,
//! and not yanking the user.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure};
use serde_json::{json, Value};

use super::bridge::{Bridge, Client};
use super::*;

fn pos(line: u64, col: u64) -> Value {
    json!({ "line": line, "col": col })
}

fn canonical(path: &Path) -> String {
    helix_stdx::path::canonicalize(path)
        .to_string_lossy()
        .into_owned()
}

fn always(mut params: Value) -> Value {
    params["interrupt"] = "always".into();
    params
}

/// The quiet time after which the user no longer counts as typing.
const IDLE: Duration = Duration::from_millis(1600);

/// Press a key that moves the cursor up or down, as the user, and wait
/// until the editor has handled it (keys and requests travel separately).
async fn press(keys: &super::bridge::Keys, c: &mut Client, key: &str) -> anyhow::Result<()> {
    let line = |st: Value| st["focused"]["selections"][0]["start"]["line"].clone();
    let before = line(c.call("editor/state", json!({})).await?);
    keys.send(key)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while line(c.call("editor/state", json!({})).await?) == before {
        ensure!(Instant::now() < deadline, "{key} was not handled");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

async fn wait_for<F: Fn() -> bool>(what: &str, f: F) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        if Instant::now() > deadline {
            bail!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

fn with_keys(toml_keys: &str) -> anyhow::Result<helix_term::config::Config> {
    let mut config = helpers::test_config();
    let raw: helix_term::config::ConfigRaw = toml::from_str(toml_keys)?;
    helix_term::keymap::merge_keys(&mut config.keys, raw.keys.unwrap());
    Ok(config)
}

// --- panes and selections ------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn views_layout_focus_close_select() -> anyhow::Result<()> {
    let a = helpers::temp_file_with_contents("one\ntwo\nthree\nfour\n")?;
    let b = helpers::temp_file_with_contents("alpha\nbeta\n")?;
    let (pa, pb) = (canonical(a.path()), canonical(b.path()));
    let mut bridge = Bridge::start(AppBuilder::new().with_file(a.path(), None))?;
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let hello = c.hello("t").await?;
            let caps = hello["capabilities"].as_array().unwrap();
            assert!(caps.contains(&json!("views")), "{caps:?}");
            assert!(caps.contains(&json!("command")), "{caps:?}");

            let list = c.call("view/list", json!({})).await?;
            let views = list["views"].as_array().unwrap();
            assert_eq!(views.len(), 1);
            let first = views[0]["viewId"].as_u64().unwrap();
            assert_eq!(views[0]["path"], pa.as_str());
            assert_eq!(views[0]["focused"], true);
            assert_eq!(views[0]["firstLine"], 1);
            assert!(views[0]["area"]["width"].as_u64().unwrap() > 10);
            assert!(views[0]["docId"].is_u64());
            // the root is always a container, as in Helix
            assert_eq!(
                list["layout"],
                json!({ "split": "vertical", "children": [{ "viewId": first }] })
            );

            // b to the right, then a below it
            let r = c
                .call(
                    "editor/open",
                    always(json!({ "path": pb, "split": "vertical" })),
                )
                .await?;
            let right = r["viewId"].as_u64().unwrap();
            let r = c
                .call(
                    "editor/open",
                    always(json!({ "path": pa, "split": "horizontal" })),
                )
                .await?;
            let low = r["viewId"].as_u64().unwrap();
            let list = c.call("view/list", json!({})).await?;
            assert_eq!(
                list["layout"],
                json!({ "split": "vertical", "children": [
                    { "viewId": first },
                    { "split": "horizontal", "children": [{ "viewId": right }, { "viewId": low }] },
                ] })
            );
            let views = list["views"].as_array().unwrap();
            let ids: Vec<u64> = views.iter().map(|v| v["viewId"].as_u64().unwrap()).collect();
            assert_eq!(ids, [first, right, low], "layout order");
            let focused: Vec<bool> = views.iter().map(|v| v["focused"] == true).collect();
            assert_eq!(focused, [false, false, true]);
            let area = |i: usize, k: &str| views[i]["area"][k].as_u64().unwrap();
            assert_eq!(area(0, "x"), 0);
            assert!(area(1, "x") > area(0, "width"), "side by side");
            assert_eq!(area(1, "x"), area(2, "x"));
            assert!(area(2, "y") > area(1, "y"), "stacked");

            // focus
            let r = c
                .call("view/focus", always(json!({ "viewId": first })))
                .await?;
            assert_eq!(r, json!({ "deferred": false }));
            let list = c.call("view/list", json!({})).await?;
            assert_eq!(list["views"][0]["focused"], true);
            let r = c.call("view/focus", json!({ "viewId": first })).await?;
            assert_eq!(r, json!({ "deferred": false }), "already focused");

            // close a pane that is not focused: nothing moves
            let r = c.call("view/close", json!({ "viewId": right })).await?;
            assert_eq!(r, json!({ "deferred": false }));
            let list = c.call("view/list", json!({})).await?;
            assert_eq!(
                list["layout"],
                json!({ "split": "vertical", "children": [{ "viewId": first }, { "viewId": low }] }),
                "a container left with one pane merges into its parent"
            );
            // close the focused pane: focus moves to the other one
            c.call("view/close", always(json!({ "viewId": first })))
                .await?;
            let list = c.call("view/list", json!({})).await?;
            assert_eq!(list["views"].as_array().unwrap().len(), 1);
            assert_eq!(list["views"][0]["viewId"], low);
            assert_eq!(list["views"][0]["focused"], true);
            let err = c.error("view/close", json!({ "viewId": low })).await?;
            assert_eq!(err["code"], 1007, "last pane: {err}");
            let err = c.error("view/focus", json!({ "viewId": 12345 })).await?;
            assert_eq!(err["code"], 1001);
            let err = c.error("view/close", json!({ "viewId": first })).await?;
            assert_eq!(err["code"], 1001, "closed pane is gone");
            let err = c.error("view/focus", json!({})).await?;
            assert_eq!(err["data"]["field"], "viewId");
            let err = c
                .error("view/focus", json!({ "viewId": low, "interrupt": "later" }))
                .await?;
            assert_eq!(err["data"]["field"], "interrupt");

            // select: two ranges, the second primary
            let r = c
                .call(
                    "editor/select",
                    always(json!({ "ranges": [
                        { "start": pos(1, 1), "end": pos(1, 4) },
                        { "start": pos(3, 1), "end": pos(3, 6) },
                    ], "primary": 1 })),
                )
                .await?;
            assert!(r["version"].is_i64(), "{r}");
            assert_eq!(r["deferred"], false);
            let sel = c.call("editor/selection", json!({})).await?;
            assert_eq!(sel["primary"], 1);
            let texts: Vec<&str> = sel["ranges"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["text"].as_str().unwrap())
                .collect();
            assert_eq!(texts, ["one", "three"]);
            // a cursor
            c.call(
                "editor/select",
                always(json!({ "ranges": [{ "start": pos(2, 3), "end": pos(2, 3) }] })),
            )
            .await?;
            let st = c.call("editor/state", json!({})).await?;
            assert_eq!(
                st["focused"]["selections"],
                json!([{ "start": pos(2, 3), "end": pos(2, 3) }])
            );

            // a pane that is not focused keeps the focus where it is
            let r = c
                .call(
                    "editor/open",
                    always(json!({ "path": pb, "split": "vertical" })),
                )
                .await?;
            let bview = r["viewId"].as_u64().unwrap();
            c.call(
                "editor/select",
                always(json!({ "viewId": low, "ranges": [{ "start": pos(4, 1), "end": pos(5, 1) }] })),
            )
            .await?;
            let st = c.call("editor/state", json!({})).await?;
            assert_eq!(st["focused"]["path"], pb.as_str());
            c.call("view/focus", always(json!({ "viewId": low })))
                .await?;
            let sel = c.call("editor/selection", json!({})).await?;
            assert_eq!(sel["ranges"][0]["text"], "four\n");
            assert_ne!(bview, low);

            for (params, field) in [
                (json!({ "ranges": [] }), "ranges"),
                (
                    json!({ "ranges": [{ "start": pos(1, 1), "end": pos(1, 1) }], "primary": 1 }),
                    "primary",
                ),
                (json!({}), "ranges"),
            ] {
                let err = c.error("editor/select", params).await?;
                assert_eq!(err["data"]["field"], field, "{err}");
            }
            let err = c
                .error(
                    "editor/select",
                    json!({ "viewId": 999, "ranges": [{ "start": pos(1, 1), "end": pos(1, 1) }] }),
                )
                .await?;
            assert_eq!(err["code"], 1001);
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

async fn open_lines(c: &mut Client, path: &str, line: u64, end_line: u64) -> anyhow::Result<Value> {
    c.call(
        "editor/open",
        always(json!({ "path": path, "line": line, "endLine": end_line })),
    )
    .await?;
    let sel = c.call("editor/selection", json!({})).await?;
    Ok(sel["ranges"][0].clone())
}

/// `endLine` without `endCol` selects whole lines, as `x` does; explicit
/// ranges and huge columns go through the same conversion.
#[tokio::test(flavor = "multi_thread")]
async fn whole_line_ranges() -> anyhow::Result<()> {
    let trailing =
        helpers::temp_file_with_contents("first\nnaïve café 🦀\nthird\nfour line, 15ch\nlast\n")?;
    let bare = helpers::temp_file_with_contents("first\nnaïve café 🦀\nlast")?;
    let (pt, pb) = (canonical(trailing.path()), canonical(bare.path()));
    let mut bridge = Bridge::start(AppBuilder::new().with_file(trailing.path(), None))?;
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let r = open_lines(&mut c, &pt, 3, 5).await?;
            assert_eq!(r["text"], "third\nfour line, 15ch\nlast\n", "{r}");
            assert_eq!((&r["start"], &r["end"]), (&pos(3, 1), &pos(6, 1)));
            let r = open_lines(&mut c, &pt, 1, 1).await?;
            assert_eq!(r["text"], "first\n");
            let r = open_lines(&mut c, &pt, 2, 2).await?;
            assert_eq!(r["text"], "naïve café 🦀\n");
            assert_eq!(r["end"], pos(3, 1));
            let r = open_lines(&mut c, &pt, 5, 5).await?;
            assert_eq!(r["text"], "last\n");
            let r = open_lines(&mut c, &pb, 3, 3).await?;
            assert_eq!(r["text"], "last", "no trailing newline: {r}");
            let r = open_lines(&mut c, &pb, 2, 3).await?;
            assert_eq!(r["text"], "naïve café 🦀\nlast");
            assert_eq!(r["end"], pos(3, 5));
            let r = open_lines(&mut c, &pb, 2, 99).await?;
            assert_eq!(r["text"], "naïve café 🦀\nlast", "past the end clamps");

            // explicit ranges, and columns no line has
            for (range, text) in [
                (json!({ "start": pos(2, 1), "end": pos(3, 1) }), "naïve café 🦀\n"),
                (json!({ "start": pos(2, 7), "end": pos(2, 11) }), "café"),
                (
                    json!({ "start": pos(1, 1), "end": { "line": 1, "col": u64::MAX } }),
                    "first",
                ),
                (
                    json!({ "start": pos(2, 1), "end": { "line": u64::MAX, "col": u64::MAX } }),
                    "naïve café 🦀\nlast",
                ),
            ] {
                c.call("editor/select", always(json!({ "ranges": [range] })))
                    .await?;
                let sel = c.call("editor/selection", json!({})).await?;
                assert_eq!(sel["ranges"][0]["text"], text, "{range}");
            }
            let t = c
                .call(
                    "editor/text",
                    json!({ "path": pt, "range": { "start": pos(4, 1), "end": { "line": 4, "col": u64::MAX } } }),
                )
                .await?;
            assert_eq!(t["text"], "four line, 15ch");
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

// --- not yanking the user ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn focus_moves_wait_while_the_user_types() -> anyhow::Result<()> {
    let a = helpers::temp_file_with_contents("one\ntwo\nthree\nfour\n")?;
    let b = helpers::temp_file_with_contents("alpha\nbeta\ngamma\n")?;
    let (pa, pb) = (canonical(a.path()), canonical(b.path()));
    let mut bridge = Bridge::start(AppBuilder::new().with_file(a.path(), None))?;

    let (pa2, pb2) = (pa.clone(), pb.clone());
    bridge
        .run(move |keys, socket| async move {
            let (pa, pb) = (pa2, pb2);
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["selection", "documents", "mode"]).await?;

            // nobody has typed yet: moves at once
            let r = c.call("editor/open", json!({ "path": pb })).await?;
            assert_eq!(r["deferred"], false);

            // a key in the last 1500 ms: the move waits
            press(&keys, &mut c, "j").await?;
            let r = c
                .call("editor/open", json!({ "path": pa, "line": 2 }))
                .await?;
            assert_eq!(r["deferred"], true, "{r}");
            assert_eq!(r["viewId"], Value::Null);
            assert_eq!(r["path"], pa.as_str());
            assert!(r["docId"].is_u64(), "a is open: {r}");
            let st = c.call("editor/state", json!({})).await?;
            assert_eq!(st["focused"]["path"], pb.as_str(), "did not move");

            // one slot, the newest wins: this command is replaced, never runs
            let r = c
                .call("editor/command", json!({ "typed": "vsplit" }))
                .await?;
            assert_eq!(r, json!({ "deferred": true }));
            let r = c
                .call("editor/open", json!({ "path": pa, "line": 3 }))
                .await?;
            assert_eq!(r["deferred"], true);
            // a refused command is refused at once, not deferred
            let err = c
                .error("editor/command", json!({ "typed": "sh true" }))
                .await?;
            assert_eq!(err["code"], 1007);

            // the user's next key clears the notice; it comes back once they pause
            press(&keys, &mut c, "k").await?;
            tokio::time::sleep(IDLE + Duration::from_millis(300)).await;
            Ok(())
        })
        .await?;
    let (status, _) = bridge.app.editor.get_status().expect("a notice");
    assert!(
        status.starts_with("bb wants to show ") && status.ends_with(":3 (:bb-go)"),
        "{status}"
    );

    let (pa2, pb2) = (pa.clone(), pb.clone());
    bridge
        .run(move |keys, socket| async move {
            let (pa, pb) = (pa2, pb2);
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["selection", "documents", "mode"]).await?;

            // :bb-go performs the newest request, and only that
            keys.send(":bb-go<ret>")?;
            let focus = c.event("event/focus").await?;
            assert_eq!(focus["path"], pa.as_str());
            let st = c.call("editor/state", json!({})).await?;
            assert_eq!(st["focused"]["selections"][0]["start"], pos(3, 1));
            let list = c.call("view/list", json!({})).await?;
            assert_eq!(
                list["views"].as_array().unwrap().len(),
                1,
                "vsplit never ran"
            );

            // insert mode waits even when quiet; "always" does not
            keys.send("i")?;
            assert_eq!(c.event("event/mode").await?["mode"], "insert");
            tokio::time::sleep(IDLE).await;
            let r = c.call("editor/open", json!({ "path": pb })).await?;
            assert_eq!(r["deferred"], true);
            let r = c.call("editor/open", always(json!({ "path": pb }))).await?;
            assert_eq!(r["deferred"], false);
            let st = c.call("editor/state", json!({})).await?;
            assert_eq!(st["focused"]["path"], pb.as_str());
            keys.send("<esc>")?;
            assert_eq!(c.event("event/mode").await?["mode"], "normal");

            // after 1500 ms of quiet, requests move at once
            tokio::time::sleep(IDLE).await;
            let r = c.call("editor/open", json!({ "path": pa })).await?;
            assert_eq!(r["deferred"], false);

            // every focus-moving method waits
            let r = c
                .call(
                    "editor/open",
                    always(json!({ "path": pb, "split": "vertical" })),
                )
                .await?;
            let right = r["viewId"].as_u64().unwrap();
            let list = c.call("view/list", json!({})).await?;
            let left = list["views"][0]["viewId"].as_u64().unwrap();
            press(&keys, &mut c, "j").await?;
            for (method, params) in [
                ("view/focus", json!({ "viewId": left })),
                ("view/close", json!({ "viewId": right })),
                (
                    "editor/select",
                    json!({ "ranges": [{ "start": pos(1, 1), "end": pos(1, 1) }] }),
                ),
                ("editor/command", json!({ "keys": "gg" })),
            ] {
                let r = c.call(method, params).await?;
                assert_eq!(r["deferred"], true, "{method}: {r}");
            }
            let r = c
                .call(
                    "editor/select",
                    json!({ "ranges": [{ "start": pos(1, 1), "end": pos(1, 1) }] }),
                )
                .await?;
            assert!(
                r["version"].is_i64(),
                "deferred select still has a version: {r}"
            );
            // closing a pane that is not focused does not move the user
            let r = c.call("view/close", json!({ "viewId": left })).await?;
            assert_eq!(r["deferred"], false);
            let list = c.call("view/list", json!({})).await?;
            assert_eq!(list["views"].as_array().unwrap().len(), 1);

            // keys and commands the bridge runs are not the user's typing
            tokio::time::sleep(IDLE).await;
            let r = c.call("editor/command", json!({ "keys": "jk" })).await?;
            assert_eq!(r["deferred"], false, "{r}");
            let r = c.call("editor/open", json!({ "path": pa })).await?;
            assert_eq!(r["deferred"], false, "the bridge's keys did not count");

            // a prompt or picker open counts as busy
            let r = c
                .call("editor/command", always(json!({ "keys": ":" })))
                .await?;
            assert_eq!(r["overlay"], "prompt");
            let r = c.call("editor/open", json!({ "path": pb })).await?;
            assert_eq!(r["deferred"], true);
            c.call("editor/command", always(json!({ "keys": "<esc>" })))
                .await?;

            // with nothing left waiting
            keys.send(":bb-go<ret>")?;
            keys.send(":bb-go<ret>")?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(())
        })
        .await?;
    let (status, _) = bridge.app.editor.get_status().expect("an error");
    assert_eq!(status, "'bb-go': Nothing from bb is waiting");
    bridge.finish().await?;
    Ok(())
}

// --- editor/command -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn command_forms_and_results() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents(
        (1..=20).map(|i| format!("line {i}\n")).collect::<String>(),
    )?;
    let path = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let line = |st: &Value| st["focused"]["selections"][0]["start"]["line"].clone();
            let cmd = |p: Value| always(p);

            // typed
            let r = c
                .call("editor/command", cmd(json!({ "typed": "vsplit" })))
                .await?;
            assert_eq!(r["deferred"], false);
            assert_eq!(r["mode"], "normal");
            assert!(r.get("overlay").is_none(), "{r}");
            let list = c.call("view/list", json!({})).await?;
            assert_eq!(list["views"].as_array().unwrap().len(), 2);
            let r = c
                .call(
                    "editor/command",
                    cmd(json!({ "typed": "echo hello %{cursor_line}" })),
                )
                .await?;
            assert_eq!(
                r["status"],
                json!({ "message": "hello 1", "severity": "info" })
            );
            let r = c
                .call("editor/command", cmd(json!({ "typed": ":5" })))
                .await?;
            assert!(r.get("status").is_none(), "{r}");
            assert_eq!(line(&c.call("editor/state", json!({})).await?), 5);
            let r = c
                .call("editor/command", cmd(json!({ "typed": "open" })))
                .await?;
            assert_eq!(r["status"]["severity"], "error", "{r}");
            for bad in ["nosuchcommand", "", " vsplit"] {
                let err = c
                    .error("editor/command", cmd(json!({ "typed": bad })))
                    .await?;
                assert_eq!(err["data"]["field"], "typed", "{bad:?}: {err}");
            }

            // static
            c.call(
                "editor/command",
                cmd(json!({ "static": "move_line_down", "count": 3 })),
            )
            .await?;
            assert_eq!(line(&c.call("editor/state", json!({})).await?), 8);
            c.call(
                "editor/command",
                cmd(json!({ "static": "goto_file_start" })),
            )
            .await?;
            assert_eq!(line(&c.call("editor/state", json!({})).await?), 1);
            let err = c
                .error("editor/command", cmd(json!({ "static": "nope" })))
                .await?;
            assert_eq!(err["data"]["field"], "static");
            let err = c
                .error(
                    "editor/command",
                    cmd(json!({ "static": "move_line_down", "count": 0 })),
                )
                .await?;
            assert_eq!(err["data"]["field"], "count");
            for waits in ["find_next_char", "replace", "select_register"] {
                let err = c
                    .error("editor/command", cmd(json!({ "static": waits })))
                    .await?;
                assert_eq!(err["code"], 1007, "{waits}");
                assert!(err["message"].as_str().unwrap().contains("keys"), "{err}");
            }
            let r = c
                .call("editor/command", cmd(json!({ "static": "insert_mode" })))
                .await?;
            assert_eq!(r["mode"], "insert");
            c.call("editor/command", cmd(json!({ "keys": "X<esc>" })))
                .await?;
            let t = c.call("editor/text", json!({ "path": path })).await?;
            assert!(t["text"].as_str().unwrap().starts_with("Xline 1\n"), "{t}");

            // keys
            let r = c
                .call("editor/command", cmd(json!({ "keys": "3j" })))
                .await?;
            assert_eq!(r["mode"], "normal");
            assert_eq!(line(&c.call("editor/state", json!({})).await?), 4);
            let r = c
                .call("editor/command", cmd(json!({ "keys": "<space>" })))
                .await?;
            assert_eq!(r["pendingKeys"], "<space>", "{r}");
            // the next request starts clean: `j` moves, it is not `<space>j`
            let r = c
                .call("editor/command", cmd(json!({ "keys": "j" })))
                .await?;
            assert!(r.get("pendingKeys").is_none(), "{r}");
            assert_eq!(line(&c.call("editor/state", json!({})).await?), 5);
            let r = c
                .call("editor/command", cmd(json!({ "keys": ":" })))
                .await?;
            assert_eq!(r["overlay"], "prompt");
            let r = c
                .call("editor/command", cmd(json!({ "keys": "goto 2" })))
                .await?;
            assert_eq!(r["overlay"], "prompt", "still typing in the prompt");
            let r = c
                .call("editor/command", cmd(json!({ "keys": "<ret>" })))
                .await?;
            assert!(r.get("overlay").is_none(), "{r}");
            assert_eq!(line(&c.call("editor/state", json!({})).await?), 2);
            let r = c
                .call("editor/command", cmd(json!({ "keys": "<space>f" })))
                .await?;
            assert_eq!(r["overlay"], "picker");
            let r = c
                .call("editor/command", cmd(json!({ "keys": "<esc>" })))
                .await?;
            assert!(r.get("overlay").is_none(), "{r}");
            let err = c
                .error("editor/command", cmd(json!({ "keys": "<nope" })))
                .await?;
            assert_eq!(err["data"]["field"], "keys");

            // the user's status message survives a command that sets none
            c.call("editor/status", json!({ "message": "mine" }))
                .await?;
            let r = c
                .call("editor/command", cmd(json!({ "typed": "noop" })))
                .await?;
            assert!(r.get("status").is_none(), "{r}");

            for params in [json!({}), json!({ "typed": "noop", "keys": "j" })] {
                let err = c.error("editor/command", params).await?;
                assert_eq!(err["code"], -32602);
            }
            Ok(())
        })
        .await?;
    assert_eq!(bridge.app.editor.get_status().unwrap().0, "mine");
    bridge.finish().await?;
    Ok(())
}

// --- the policy, as an attacker would try it ---------------------------------------------

struct Marker(PathBuf);

impl Marker {
    fn exists(&self) -> bool {
        self.0.exists()
    }
}

fn marker(dir: &Path, name: &str) -> Marker {
    Marker(dir.join(name))
}

/// User bindings that run a shell, as someone might have in their config.
fn shell_bindings(m: &str) -> String {
    format!(
        r#"
[keys.normal]
X = ":sh touch {m}"
Y = "shell_pipe"
Z = ["move_line_down", ":run-shell-command touch {m}"]
M = "@|touch {m}<ret>"
E = ":echo %sh{{touch {m}}}"
[keys.normal.g]
H = ":sh touch {m}"
[keys.insert]
C-x = ":sh touch {m}"
"#
    )
}

async fn refused(c: &mut Client, params: Value) -> anyhow::Result<Value> {
    let response = c.request("editor/command", always(params.clone())).await?;
    let Some(err) = response.get("error") else {
        bail!("{params} should have been refused: {response}");
    };
    ensure!(err["code"] == 1007, "{params}: {err}");
    Ok(err.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn safe_policy_refuses_every_route_to_a_shell() -> anyhow::Result<()> {
    let dir = super::bridge::bridge_dir()?;
    let m = marker(dir.path(), "pwned");
    let ms = m.0.to_string_lossy().into_owned();
    let file = helpers::temp_file_with_contents("one\ntwo\nthree\n")?;
    let mut bridge = Bridge::start(
        AppBuilder::new()
            .with_config(with_keys(&shell_bindings(&ms))?)
            .with_file(file.path(), None),
    )?;
    let m0 = m.0.clone();
    bridge
        .run(move |keys, socket| async move {
            let m = Marker(m0);
            let mut c = Client::connect(&socket).await?;
            assert_eq!(c.hello("t").await?["commandPolicy"], "safe");
            let t = format!("touch {ms}");

            // typed: names, aliases, expansions at any depth
            for line in [
                format!("sh {t}"),
                format!("! {t}"),
                format!("run-shell-command {t}"),
                "| sort".into(),
                "pipe sort".into(),
                "pipe-to cat".into(),
                "insert-output echo".into(),
                "append-output echo".into(),
                format!("echo %sh{{{t}}}"),
                format!("echo \"%sh{{{t}}}\""),
                format!("echo \"%{{cursor_line}} %sh|{t}|\""),
                format!("echo \"%%%sh({t})\""),
                format!("echo %sh{{echo %sh{{{t}}}}}"),
                format!("open %sh{{{t}}}"),
                format!("noop %sh[{t}]"),
                format!("set-register a %sh<{t}>"),
                format!("goto %sh{{{t}}}"),
                "set shell [\"sh\", \"-c\", \"touch /tmp/x\"]".into(),
                "set-option bridge.commands all".into(),
                "set BRIDGE.Commands all".into(),
                "set bridge {\"commands\": \"all\", \"enable\": true}".into(),
                "toggle bridge.commands safe all".into(),
                "set clipboard-provider {\"custom\": {}}".into(),
                "set terminal.command sh".into(),
                "set workspace-trust.level always".into(),
                "workspace-trust".into(),
                "workspace-untrust".into(),
                "config-reload".into(),
                "lsp-workspace-command x".into(),
                "debug-start".into(),
                "dbg".into(),
                "debug-eval 1".into(),
                "bb-send hi".into(),
                "bb-ask why".into(),
            ] {
                let err = refused(&mut c, json!({ "typed": line })).await?;
                assert!(err["data"]["reason"].is_string(), "{line}: {err}");
                assert!(!m.exists(), "{line} ran a shell");
            }
            // an expansion's value is checked, not just its text
            c.call(
                "editor/command",
                always(json!({ "typed": "set-register a shell" })),
            )
            .await?;
            refused(&mut c, json!({ "typed": "set %reg{a} [\"sh\"]" })).await?;
            assert_eq!(c.hello("t").await?["commandPolicy"], "safe");

            // static
            for name in [
                "shell_pipe",
                "shell_pipe_to",
                "shell_insert_output",
                "shell_append_output",
                "shell_keep_pipe",
                "replay_macro",
                "command_palette",
                "suspend",
                "dap_launch",
                "dap_restart",
            ] {
                refused(&mut c, json!({ "static": name })).await?;
            }

            // keys: default bindings, the user's own bindings, the command
            // line, the palette, macros
            c.call(
                "editor/command",
                always(json!({ "typed": format!("set-register @ |{t}<ret>") })),
            )
            .await?;
            for keys in [
                format!("|{t}<ret>"),
                format!("!{t}<ret>"),
                format!("<A-|>{t}<ret>"),
                format!("<A-!>{t}<ret>"),
                format!("${t}<ret>"),
                format!(":sh {t}<ret>"),
                format!(":!{t}<ret>"),
                format!(":run-shell-command {t}<ret>"),
                format!(":echo %sh{{{t}}}<ret>"),
                format!(":echo \"%sh{{{t}}}\"<ret>"),
                ":<ret>".into(),
                ":set bridge.commands all<ret>".into(),
                "X".into(),
                "Y".into(),
                "Z".into(),
                "M".into(),
                "E".into(),
                "gH".into(),
                "jjgH".into(),
                "ihello<C-x>".into(),
                "<space>?".into(),
                "q".into(),
                "\"@q".into(),
                format!("Q|{t}<ret>Qq"),
            ] {
                let err = refused(&mut c, json!({ "keys": keys })).await?;
                assert!(err["data"]["key"].is_string(), "{keys}: {err}");
                assert!(err["data"]["sent"].is_u64(), "{keys}: {err}");
                // Escape was sent: nothing is left open
                let r = c
                    .call("editor/command", always(json!({ "keys": "<esc>" })))
                    .await?;
                assert!(r.get("overlay").is_none(), "{keys}: {r}");
                assert_eq!(r["mode"], "normal", "{keys}: {r}");
                assert!(!m.exists(), "{keys} ran a shell");
            }
            let err = refused(&mut c, json!({ "keys": format!("jj:sh {t}<ret>") })).await?;
            assert_eq!(
                err["data"]["sent"],
                2 + 1 + t.len() + 3,
                "stopped at Enter: {err}"
            );

            // a shell prompt the user left open: typing is fine, Enter is not
            keys.send("|")?;
            wait_overlay(&mut c, "prompt").await?;
            let err = refused(&mut c, json!({ "keys": format!("{t}<ret>") })).await?;
            assert!(
                err["message"].as_str().unwrap().contains("did not open"),
                "{err}"
            );
            let r = c
                .call("editor/command", always(json!({ "keys": "<esc>" })))
                .await?;
            assert!(r.get("overlay").is_none(), "{r}");
            // ... and a prompt the request opened itself is fine
            c.call("editor/command", always(json!({ "keys": "/two<ret>" })))
                .await?;

            // `.` repeats an insert whose keys ran a shell binding (the user's)
            keys.send("i<C-x><esc>")?;
            wait_for("the user's binding", || m.exists()).await?;
            wait_mode(&mut c, "normal").await?;
            std::fs::remove_file(&m.0)?;
            refused(&mut c, json!({ "keys": "." })).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(!m.exists(), ". ran a shell");
            Ok(())
        })
        .await?;
    assert!(!m.exists());
    bridge.finish().await?;
    Ok(())
}

async fn wait_overlay(c: &mut Client, overlay: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let r = c
            .call("editor/command", always(json!({ "typed": "noop" })))
            .await?;
        if r["overlay"] == overlay {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "no {overlay}: {r}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_mode(c: &mut Client, mode: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while c.call("editor/state", json!({})).await?["mode"] != mode {
        ensure!(Instant::now() < deadline, "not in {mode}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn safe_policy_keeps_unsaved_work() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("one\ntwo\n")?;
    let other = helpers::temp_file_with_contents("other\n")?;
    let path = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    let other_path = canonical(other.path());
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            // reload is fine while nothing is modified
            c.call("editor/command", always(json!({ "typed": "reload" })))
                .await?;
            c.call("editor/command", always(json!({ "typed": "rla" })))
                .await?;
            c.call(
                "editor/command",
                always(json!({ "keys": "iunsaved <esc>" })),
            )
            .await?;
            c.call(
                "editor/open",
                always(json!({ "path": other_path, "split": "vertical" })),
            )
            .await?;
            let first = c.call("view/list", json!({})).await?["views"][0]["viewId"].clone();
            c.call("view/focus", always(json!({ "viewId": first })))
                .await?;
            for line in [
                "q!",
                "quit!",
                "qa!",
                "quit-all!",
                "cq!",
                "cquit!",
                "bc!",
                "bclose!",
                "buffer-close!",
                "bca!",
                "buffer-close-all!",
                "bco!",
                "buffer-close-others!",
                "wq!",
                "write-quit!",
                "wqa!",
                "xa!",
                "write-quit-all!",
                "x!",
                "exit!",
                "w!",
                "write!",
                "wa!",
                "write-all!",
                "wbc!",
                "write-buffer-close!",
                "reload",
                "rl",
                "reload-all",
                "rla",
            ] {
                refused(&mut c, json!({ "typed": line })).await?;
                refused(&mut c, json!({ "keys": format!(":{line}<ret>") })).await?;
            }
            // from the other, unmodified buffer, reload-all still sees the modified one
            refused(&mut c, json!({ "keys": "<C-w>w:rla<ret>" })).await?;
            let st = c.call("editor/state", json!({})).await?;
            let doc = st["documents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["path"] == path.as_str())
                .unwrap()
                .clone();
            assert_eq!(doc["modified"], true, "{st}");
            let t = c.call("editor/text", json!({ "path": path })).await?;
            assert_eq!(t["text"], "unsaved one\ntwo\n");
            // plain quit refuses by itself, as in Helix
            c.call("editor/command", always(json!({ "typed": "qa" })))
                .await?;
            assert!(
                c.call("editor/state", json!({})).await.is_ok(),
                "still running"
            );
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_all_runs_everything_and_none_nothing() -> anyhow::Result<()> {
    let dir = super::bridge::bridge_dir()?;
    let (m1, m2) = (marker(dir.path(), "typed"), marker(dir.path(), "bound"));
    let mut config = with_keys(&shell_bindings(&m2.0.to_string_lossy()))?;
    config.editor.bridge.commands = helix_view::editor::BridgeCommandPolicy::All;
    let mut bridge = Bridge::start(AppBuilder::new().with_config(config))?;
    let (p1, p2) = (m1.0.clone(), m2.0.clone());
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            assert_eq!(c.hello("t").await?["commandPolicy"], "all");
            c.call(
                "editor/command",
                always(json!({ "typed": format!("sh touch {}", p1.display()) })),
            )
            .await?;
            wait_for("typed :sh", || p1.exists()).await?;
            c.call("editor/command", always(json!({ "keys": "X" })))
                .await?;
            wait_for("bound :sh", || p2.exists()).await?;
            // under `all` even this runs; it tightens the policy
            c.call(
                "editor/command",
                always(json!({ "typed": "set bridge.commands none" })),
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(c.hello("t").await?["commandPolicy"], "none");
            let err = c
                .error("editor/command", always(json!({ "typed": "noop" })))
                .await?;
            assert_eq!(err["code"], 1007);
            assert!(
                err["message"].as_str().unwrap().contains("disabled"),
                "{err}"
            );
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_set_at_start_cannot_be_loosened() -> anyhow::Result<()> {
    let mut config = helpers::test_config();
    config.editor.bridge.commands = helix_view::editor::BridgeCommandPolicy::None;
    let mut bridge = Bridge::start(AppBuilder::new().with_config(config))?;
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            assert_eq!(c.hello("t").await?["commandPolicy"], "none");
            for params in [
                json!({ "typed": "noop" }),
                json!({ "static": "no_op" }),
                json!({ "keys": "j" }),
            ] {
                let err = c.error("editor/command", always(params)).await?;
                assert_eq!(err["code"], 1007);
            }
            Ok(())
        })
        .await?;
    // as `:config-reload` would after an agent edited config.toml
    bridge
        .app
        .editor
        .config_events
        .0
        .send(helix_view::editor::ConfigEvent::Update(Box::new({
            let mut editor = (*bridge.app.editor.config()).clone();
            editor.bridge.commands = helix_view::editor::BridgeCommandPolicy::All;
            editor
        })))?;
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(c.hello("t").await?["commandPolicy"], "none");
            let err = c
                .error("editor/command", always(json!({ "typed": "noop" })))
                .await?;
            assert_eq!(err["code"], 1007);
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}
