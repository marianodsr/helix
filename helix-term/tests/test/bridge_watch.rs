//! bb bridge: Helix watches the files of its open buffers (capability
//! `watch`, macOS). Drives a real `Application` and changes files on disk
//! the way agents, editors and git do.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure};
use serde_json::{json, Value};

use super::bridge::{until_status, Bridge, Client};
use super::*;

const WAIT: Duration = Duration::from_secs(5);

fn canonical(path: &Path) -> String {
    helix_stdx::path::canonicalize(path)
        .to_string_lossy()
        .into_owned()
}

/// A different mtime from the previous write.
fn write(path: &str, text: &str) -> anyhow::Result<()> {
    std::thread::sleep(Duration::from_millis(2));
    std::fs::write(path, text)?;
    Ok(())
}

/// What editors and formatters do: write a temporary file, rename it over.
fn write_by_rename(path: &str, text: &str) -> anyhow::Result<()> {
    std::thread::sleep(Duration::from_millis(2));
    let tmp = format!("{path}.tmp~");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn text(c: &mut Client, path: &str) -> anyhow::Result<Value> {
    c.call("editor/text", json!({ "path": path })).await
}

/// Poll the buffer (not the disk) until it holds `want`.
async fn until_text(c: &mut Client, path: &str, want: &str) -> anyhow::Result<Duration> {
    let start = Instant::now();
    loop {
        let t = text(c, path).await?;
        if t["text"] == want && t["version"].is_number() {
            return Ok(start.elapsed());
        }
        if start.elapsed() > WAIT {
            bail!("the buffer of {path} never held {want:?}: {t}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Notifications for `dur`, by method.
async fn events(c: &mut Client, dur: Duration, method: &str) -> anyhow::Result<Vec<Value>> {
    Ok(c.drain(dur)
        .await?
        .into_iter()
        .filter(|m| m["method"] == method)
        .map(|m| m["params"].clone())
        .collect())
}

fn watched() -> usize {
    helix_term::bridge::watch_counts().0
}

async fn until_watched(n: usize) -> anyhow::Result<()> {
    let start = Instant::now();
    while watched() != n {
        ensure!(
            start.elapsed() < WAIT,
            "watching {} files, not {n}",
            watched()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn written_in_place_or_by_rename_the_buffer_follows() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("one\ntwo\nthree\n")?;
    let p = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let caps = c.hello("t").await?["capabilities"].clone();
            assert!(caps.as_array().unwrap().contains(&json!("watch")), "{caps}");
            c.subscribe(&["documents"]).await?;
            until_watched(1).await?;

            write(&p, "one\nTWO\nthree\n")?;
            until_text(&mut c, &p, "one\nTWO\nthree\n").await?;
            let ev = c.event("event/reloaded").await?;
            assert_eq!(ev["origin"], "watch");
            assert_eq!(ev["path"], p.as_str());
            assert_eq!(
                ev["ranges"],
                json!([{ "start": { "line": 2, "col": 1 }, "end": { "line": 2, "col": 4 } }])
            );
            assert_eq!(ev["removed"], 1);
            assert_eq!(text(&mut c, &p).await?["modified"], false);

            // replaced by rename, twice: the watch follows the new file
            for round in ["first", "second"] {
                let want = format!("{round} rename\n");
                write_by_rename(&p, &want)?;
                until_text(&mut c, &p, &want).await?;
                assert_eq!(c.event("event/reloaded").await?["origin"], "watch");
            }
            // and in place again after the renames
            write(&p, "in place\n")?;
            until_text(&mut c, &p, "in place\n").await?;
            Ok(())
        })
        .await?;
    // `u` undoes a watched reload like any reload
    bridge
        .run(|keys, _| async move {
            keys.send("u")?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await?;
    assert_eq!(
        helix_view::doc!(bridge.app.editor).text().to_string(),
        "second rename\n"
    );
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn deleted_then_created_again() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("here\n")?;
    let p = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    let p2 = p.clone();
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            until_watched(1).await?;
            std::fs::remove_file(&p2)?;
            until_status(&format!(
                "{} deleted on disk; the buffer is kept (:w writes it again)",
                helix_stdx::path::get_relative_path(Path::new(&p2)).display()
            ))
            .await?;
            assert_eq!(text(&mut c, &p2).await?["text"], "here\n", "kept");
            write(&p2, "back again\n")?;
            until_text(&mut c, &p2, "back again\n").await?;
            write(&p2, "and changed\n")?;
            until_text(&mut c, &p2, "and changed\n").await?;
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

/// A buffer with unsaved changes is never touched; the user is told once.
/// Helix's own saves reload nothing.
#[tokio::test(flavor = "multi_thread")]
async fn unsaved_work_and_own_saves() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("mine\n")?;
    let p = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    let p2 = p.clone();
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["documents", "externalChange"]).await?;
            until_watched(1).await?;

            // own saves, several in a row: no reload, no external change
            for i in 0..5 {
                keys.send(&format!("ggiedit{i} <esc>:w<ret>"))?;
                let saved = c.event("event/documentSaved").await?;
                assert_eq!(saved["path"], p2.as_str());
            }
            let rest = c.drain(Duration::from_millis(300)).await?;
            assert!(
                !rest
                    .iter()
                    .any(|m| m["method"] == "event/reloaded"
                        || m["method"] == "event/externalChange"),
                "{rest:?}"
            );

            // unsaved work: someone else writes the file, twice
            keys.send("ggiunsaved <esc>")?;
            c.event("event/documentChanged").await?;
            write(&p2, "theirs\n")?;
            let ev = c.event("event/externalChange").await?;
            assert_eq!(ev["path"], p2.as_str());
            let t = text(&mut c, &p2).await?;
            assert!(t["text"].as_str().unwrap().starts_with("unsaved "), "{t}");
            assert_eq!(t["modified"], true);
            // the same change is not announced twice
            let again = events(&mut c, Duration::from_millis(200), "event/externalChange").await?;
            assert!(again.is_empty(), "{again:?}");
            write(&p2, "theirs again\n")?;
            c.event("event/externalChange").await?;
            Ok(())
        })
        .await?;
    let (status, _) = bridge.app.editor.get_status().expect("a warning");
    assert!(status.contains("changed on disk"), "{status}");
    assert!(helix_view::doc!(bridge.app.editor).is_modified());
    bridge.finish().await?;
    Ok(())
}

/// An agent writing a file many times: every final state is shown.
#[tokio::test(flavor = "multi_thread")]
async fn bursts_of_writes_end_on_the_last_one() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("0\n")?;
    let p = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["documents"]).await?;
            until_watched(1).await?;
            // 20 ms apart: coalesced, the last one wins
            let mut text_now = String::new();
            for i in 1..=10 {
                text_now = (0..=i).map(|n| format!("{n}\n")).collect();
                write_by_rename(&p, &text_now)?;
                std::thread::sleep(Duration::from_millis(20));
            }
            until_text(&mut c, &p, &text_now).await?;
            let reloads = events(&mut c, Duration::from_millis(200), "event/reloaded").await?;
            assert!(reloads.len() < 10, "coalesced: {}", reloads.len());
            // spread out: each one shown
            for i in 0..4 {
                let want = format!("step {i}\n");
                write(&p, &want)?;
                until_text(&mut c, &p, &want).await?;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

/// What git does on a checkout: many open files change at once.
#[tokio::test(flavor = "multi_thread")]
async fn twenty_files_at_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let paths: Vec<PathBuf> = (0..20)
        .map(|i| {
            let path = dir.path().join(format!("f{i}.txt"));
            std::fs::write(&path, format!("old {i}\n")).unwrap();
            path
        })
        .collect();
    let mut builder = AppBuilder::new();
    for path in &paths {
        builder = builder.with_file(path, None);
    }
    let mut bridge = Bridge::start(builder)?;
    let names: Vec<String> = paths.iter().map(|p| canonical(p)).collect();
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            until_watched(20).await?;
            std::thread::sleep(Duration::from_millis(2));
            for (i, p) in names.iter().enumerate() {
                write_by_rename(p, &format!("new {i}\n"))?;
            }
            for (i, p) in names.iter().enumerate() {
                until_text(&mut c, p, &format!("new {i}\n")).await?;
            }
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

/// The watch follows the open buffers: a buffer written to a new path, a
/// path through a symlinked directory, a closed buffer.
#[tokio::test(flavor = "multi_thread")]
async fn follows_paths_symlinks_and_closes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let real = dir.path().join("real");
    std::fs::create_dir(&real)?;
    std::fs::write(real.join("x.txt"), "x\n")?;
    std::os::unix::fs::symlink(&real, dir.path().join("link"))?;
    let via_link = canonical(&dir.path().join("link").join("x.txt"));
    let via_real = real.join("x.txt").to_string_lossy().into_owned();
    let elsewhere = canonical(&dir.path().join("elsewhere.txt"));
    let mut bridge = Bridge::start(AppBuilder::new())?;
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.call(
                "editor/open",
                json!({ "path": via_link, "interrupt": "always" }),
            )
            .await?;
            until_watched(1).await?;
            // written through the real path
            write_by_rename(&via_real, "through the real path\n")?;
            until_text(&mut c, &via_link, "through the real path\n").await?;

            // `:w <other>` gives the buffer a new file to watch
            // pasted, not typed: completing a long path on every typed
            // character is slow on a loaded machine
            keys.send(":w ")?;
            keys.event(termina::event::Event::Paste(elsewhere.clone()))?;
            keys.send("<ret>")?;
            let start = Instant::now();
            while !Path::new(&elsewhere).exists() {
                ensure!(start.elapsed() < WAIT, ":w did not write");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            write(&elsewhere, "the new file changed\n")?;
            until_text(&mut c, &elsewhere, "the new file changed\n").await?;

            // closing the buffer stops watching
            keys.send(":bc!<ret>")?;
            until_watched(0).await?;
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

/// `[editor.bridge] watch = false`: nothing is watched or listed; the focus
/// check still works.
#[tokio::test(flavor = "multi_thread")]
async fn off_switch_and_the_focus_check() -> anyhow::Result<()> {
    use termina::event::Event;
    let file = helpers::temp_file_with_contents("a\n")?;
    let p = canonical(file.path());
    let mut config = helpers::test_config();
    config.editor.bridge.watch = false;
    let mut bridge = Bridge::start(
        AppBuilder::new()
            .with_config(config)
            .with_file(file.path(), None),
    )?;
    let p2 = p.clone();
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let caps = c.hello("t").await?["capabilities"].clone();
            assert!(
                !caps.as_array().unwrap().contains(&json!("watch")),
                "{caps}"
            );
            assert_eq!(watched(), 0);
            c.subscribe(&["documents"]).await?;
            write(&p2, "b\n")?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(text(&mut c, &p2).await?["text"], "a\n", "not watched");
            keys.event(Event::FocusIn)?;
            until_text(&mut c, &p2, "b\n").await?;
            assert_eq!(c.event("event/reloaded").await?["origin"], "focus");
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_focus_check_does_not_reload_twice() -> anyhow::Result<()> {
    use termina::event::Event;
    let file = helpers::temp_file_with_contents("a\n")?;
    let p = canonical(file.path());
    let mut bridge = Bridge::start(AppBuilder::new().with_file(file.path(), None))?;
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["documents"]).await?;
            until_watched(1).await?;
            write(&p, "b\n")?;
            until_text(&mut c, &p, "b\n").await?;
            keys.event(Event::FocusIn)?;
            let reloads = events(&mut c, Duration::from_millis(300), "event/reloaded").await?;
            assert_eq!(reloads.len(), 1, "{reloads:?}");
            assert_eq!(reloads[0]["origin"], "watch");
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}
