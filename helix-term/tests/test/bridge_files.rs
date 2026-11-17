//! bb bridge: `:bb-files`. With a client subscribed to `files` it only
//! emits `event/files`; without one it runs the file manager, which needs a
//! terminal and is checked by `bb-bridge/bench/live.py verify` under a pty.

use std::time::Duration;

use serde_json::{json, Value};

use super::bridge::{until_status, Bridge, Client};
use super::*;

fn canonical(path: &std::path::Path) -> String {
    helix_stdx::path::canonicalize(path)
        .to_string_lossy()
        .into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn files_event_in_normal_and_select_mode() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("one\ntwo\n")?;
    let path = canonical(file.path());
    let mut config = helpers::test_config();
    // never run: a client is listening
    config.editor.bridge.file_manager = vec!["/nonexistent/file-manager".into()];
    let mut bridge = Bridge::start(
        AppBuilder::new()
            .with_config(config)
            .with_file(file.path(), None),
    )?;
    let p = path.clone();
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let subs = c.subscribe(&["files"]).await?;
            assert_eq!(subs["subscribed"], json!(["files"]));
            let state = c.call("editor/state", json!({})).await?;

            keys.send(":bb-files<ret>")?;
            let ev = c.event("event/files").await?;
            assert_eq!(ev["path"], p.as_str());
            assert_eq!(ev["docId"], state["focused"]["docId"]);
            assert!(ev["cwd"].as_str().is_some_and(|cwd| !cwd.is_empty()));

            // select mode, as a key binding would run it
            keys.send("v:bb-files<ret>")?;
            let ev = c.event("event/files").await?;
            assert_eq!(ev["path"], p.as_str());
            let st = c.call("editor/state", json!({})).await?;
            assert_eq!(st["mode"], "select");
            keys.send("<esc>")?;

            // a scratch buffer has no path
            keys.send(":new<ret>:bb-files<ret>")?;
            let ev = c.event("event/files").await?;
            assert_eq!(ev["path"], Value::Null);
            Ok(())
        })
        .await?;
    // nothing else happened: no error, the file manager never ran
    let status = bridge.app.editor.get_status().map(|(m, _)| m.to_string());
    assert!(
        status.as_deref().is_none_or(|s| !s.contains("not found")),
        "{status:?}"
    );
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn files_without_client_missing_program_is_an_error() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("one\n")?;
    let mut config = helpers::test_config();
    config.editor.bridge.file_manager =
        vec!["helix-bb-no-such-file-manager".into(), "{chooser}".into()];
    let mut bridge = Bridge::start(
        AppBuilder::new()
            .with_config(config)
            .with_file(file.path(), None),
    )?;
    bridge
        .run(|keys, socket| async move {
            // connected, but not subscribed to `files`
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["send"]).await?;
            keys.send(":bb-files<ret>")?;
            until_status("'bb-files': helix-bb-no-such-file-manager not found ([editor.bridge] file-manager)").await?;
            let rest = c.drain(Duration::from_millis(200)).await?;
            assert!(rest.is_empty(), "{rest:?}");
            Ok(())
        })
        .await?;
    let (_, severity) = bridge.app.editor.get_status().unwrap();
    assert_eq!(*severity, helix_core::diagnostic::Severity::Error);
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn files_refused_through_editor_command_under_safe() -> anyhow::Result<()> {
    let mut bridge = Bridge::start(AppBuilder::new())?;
    bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["files"]).await?;
            let err = c
                .error(
                    "editor/command",
                    json!({ "typed": "bb-files", "interrupt": "always" }),
                )
                .await?;
            assert_eq!(err["code"], 1007);
            assert_eq!(err["data"]["command"], "bb-files");
            let rest = c.drain(Duration::from_millis(200)).await?;
            assert!(rest.is_empty(), "refused: no event {rest:?}");
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}
