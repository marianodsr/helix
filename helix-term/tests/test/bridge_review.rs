//! bb bridge step 5: review/set, review/clear, review/state, `:bb-review`,
//! `:bb-diff` and the inline diff, against a real git repository.
//! bb-bridge/bench/look.py review scenes capture how it looks.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use helix_core::text_annotations::TextAnnotations;
use helix_core::visual_offset_from_anchor;
use helix_view::annotations::bridge::inline_diff;
use helix_view::{doc, view, Editor};
use serde_json::{json, Value};

use super::bridge::{until_overlay, until_status, Bridge, Client};
use super::*;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env("GIT_AUTHOR_NAME", "a")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "c")
        .env("GIT_COMMITTER_EMAIL", "c@example.com")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A repository: `base` is the first commit, HEAD the second, and the
/// work tree has uncommitted changes on top.
struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    base: String,
}

impl Repo {
    fn new() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        git(&root, &["init", "-q"]);
        let lines = |n: usize, tag: &str| -> String {
            (1..=n).map(|i| format!("{tag} line {i}\n")).collect()
        };
        std::fs::write(root.join("a.txt"), lines(20, "a")).unwrap();
        std::fs::write(root.join("old.txt"), "moved\n").unwrap();
        std::fs::write(root.join("gone.txt"), "deleted one\ndeleted two\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-qm", "base"]);
        let base = git(&root, &["rev-parse", "HEAD"]);
        // committed after the base: line 5 changed, line 10 removed
        let text = lines(20, "a")
            .replace("a line 5\n", "a line FIVE\n")
            .replace("a line 10\n", "");
        std::fs::write(root.join("a.txt"), text).unwrap();
        git(&root, &["commit", "-qam", "head"]);
        // uncommitted: a line added at the end
        let mut text = std::fs::read_to_string(root.join("a.txt")).unwrap();
        text.push_str("a line added\n");
        std::fs::write(root.join("a.txt"), text).unwrap();
        git(&root, &["mv", "old.txt", "new.txt"]);
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::write(root.join("added.txt"), "fresh one\nfresh two\n").unwrap();
        Repo {
            _dir: dir,
            root,
            base,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn s(&self, rel: &str) -> String {
        self.path(rel).to_string_lossy().into_owned()
    }

    fn files(&self) -> Value {
        json!([
            { "path": self.s("a.txt"), "status": "modified", "additions": 2, "deletions": 2 },
            { "path": self.s("new.txt"), "status": "renamed", "oldPath": self.s("old.txt"), "additions": 0, "deletions": 0 },
            { "path": self.s("gone.txt"), "status": "deleted", "additions": 0, "deletions": 2 },
            { "path": self.s("added.txt"), "status": "added", "additions": 2, "deletions": 0 },
        ])
    }
}

/// The base Helix diffs the document at `path` against, if any.
fn base_of(editor: &Editor, path: &Path) -> Option<String> {
    let doc = editor.document_by_path(path)?;
    let handle = doc.diff_handle()?;
    let diff = handle.load();
    Some(diff.diff_base().to_string())
}

/// (before, after) line ranges of the document's hunks.
fn hunks_of(editor: &Editor, path: &Path) -> Vec<(std::ops::Range<u32>, std::ops::Range<u32>)> {
    let Some(handle) = editor.document_by_path(path).and_then(|d| d.diff_handle()) else {
        return Vec::new();
    };
    let diff = handle.load();
    (0..diff.len())
        .map(|i| {
            let h = diff.nth_hunk(i);
            (h.before, h.after)
        })
        .collect()
}

/// Visual row of the start of 0-based `line` in the focused view, counting
/// the virtual lines above it.
fn row_of_line(editor: &Editor, line: usize) -> usize {
    let view = view!(editor);
    let doc = doc!(editor);
    let text = doc.text().slice(..);
    let fmt = doc.text_format(view.inner_width(doc), None);
    let annotations: TextAnnotations = view.text_annotations(doc, None);
    let (pos, _) =
        visual_offset_from_anchor(text, 0, text.line_to_char(line), &fmt, &annotations, 10_000)
            .expect("visible");
    pos.row
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(400)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn review_sets_the_base_and_clears_it() -> anyhow::Result<()> {
    let repo = Repo::new();
    let a = repo.path("a.txt");
    let mut bridge = Bridge::start(AppBuilder::new().with_file(&a, None))?;
    // before: HEAD
    bridge
        .run(|_, _| async {
            settle().await;
            Ok(())
        })
        .await?;
    assert_eq!(
        hunks_of(&bridge.app.editor, &a).len(),
        1,
        "only the uncommitted line"
    );

    let (files, base) = (repo.files(), repo.base.clone());
    let (added, new) = (repo.path("added.txt"), repo.path("new.txt"));
    let result = bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            let hello = c.hello("t").await?;
            assert!(hello["capabilities"].as_array().unwrap().contains(&json!("review")));
            let r = c
                .call(
                    "review/set",
                    json!({ "id": "r1", "title": "Branch", "base": { "rev": base }, "files": files }),
                )
                .await?;
            settle().await;
            let state = c.call("review/state", json!({})).await?;
            Ok((r, state))
        })
        .await?;
    let (r, state) = result;
    assert_eq!(
        r,
        json!({ "files": 4, "base": repo.base, "deferred": false })
    );
    assert_eq!(
        state,
        json!({ "active": true, "id": "r1", "title": "Branch", "base": repo.base, "inline": true, "files": 4 })
    );
    assert!(inline_diff());
    // the open buffer now compares with the base commit
    let at_base: String = (1..=20).map(|i| format!("a line {i}\n")).collect();
    assert_eq!(
        base_of(&bridge.app.editor, &a).as_deref(),
        Some(at_base.as_str())
    );
    assert_eq!(hunks_of(&bridge.app.editor, &a).len(), 3);

    // files opened later get the review's base: a rename, a new file
    bridge
        .run({
            let (added, new) = (added.clone(), new.clone());
            move |_, socket| async move {
                let mut c = Client::connect(&socket).await?;
                for path in [&new, &added] {
                    c.call(
                        "editor/open",
                        json!({ "path": path, "interrupt": "always" }),
                    )
                    .await?;
                }
                settle().await;
                Ok(())
            }
        })
        .await?;
    assert_eq!(
        base_of(&bridge.app.editor, &new).as_deref(),
        Some("moved\n")
    );
    assert_eq!(base_of(&bridge.app.editor, &added).as_deref(), Some(""));
    assert_eq!(hunks_of(&bridge.app.editor, &added), [(0..0, 0..2)]);

    // the removed lines take virtual rows above their change
    let a_s = repo.s("a.txt");
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.call("editor/open", json!({ "path": a_s, "interrupt": "always" }))
                .await?;
            settle().await;
            Ok(())
        })
        .await?;
    {
        let editor = &bridge.app.editor;
        // line 5 changed: its old text is one row above it; line 10 removed
        assert_eq!(row_of_line(editor, 4), 5);
        assert_eq!(row_of_line(editor, 9), 11);
    }

    // back to HEAD; the new file loses its diff
    bridge
        .run(|_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            assert_eq!(c.call("review/clear", json!({})).await?, json!({}));
            settle().await;
            let state = c.call("review/state", json!({})).await?;
            assert_eq!(state["active"], false);
            assert_eq!(state["inline"], false);
            Ok(())
        })
        .await?;
    let head = git(&repo.root, &["show", "HEAD:a.txt"]) + "\n";
    assert_eq!(
        base_of(&bridge.app.editor, &a).as_deref(),
        Some(head.as_str())
    );
    assert_eq!(base_of(&bridge.app.editor, &added), None);
    assert!(!inline_diff());
    // no virtual rows without the inline diff
    assert_eq!(row_of_line(&bridge.app.editor, 9), 9);
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn editing_during_a_review_follows_and_undoes() -> anyhow::Result<()> {
    let repo = Repo::new();
    let a = repo.path("a.txt");
    let mut bridge = Bridge::start(AppBuilder::new().with_file(&a, None))?;
    let (files, base) = (repo.files(), repo.base.clone());
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.call(
                "review/set",
                json!({ "id": "r", "base": { "rev": base }, "files": files }),
            )
            .await?;
            settle().await;
            // a line typed at the top is a new change; undo takes it away
            keys.send("ggOtyped<esc>")?;
            settle().await;
            Ok(())
        })
        .await?;
    let hunks = hunks_of(&bridge.app.editor, &a);
    assert_eq!(hunks[0], (0..0, 0..1), "{hunks:?}");
    assert_eq!(hunks.len(), 4);
    // the typed text is in the document, not in a virtual line
    assert_eq!(
        doc!(bridge.app.editor).text().line(0).to_string(),
        "typed\n"
    );
    bridge
        .run(|keys, _| async move {
            keys.send("u")?;
            settle().await;
            // ]g walks Helix's own diff, which follows the review
            keys.send("gg]g")?;
            settle().await;
            Ok(())
        })
        .await?;
    let editor = &bridge.app.editor;
    assert_eq!(hunks_of(editor, &a).len(), 3);
    let doc = doc!(editor);
    let text = doc.text().slice(..);
    let line = text.char_to_line(doc.selection(view!(editor).id).primary().cursor(text));
    assert_eq!(line, 4, "]g goes to the changed line 5");
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_errors_events_and_commands() -> anyhow::Result<()> {
    let repo = Repo::new();
    let a = repo.path("a.txt");
    let mut bridge = Bridge::start(AppBuilder::new().with_file(&a, None))?;
    let files = repo.files();
    let a_s = repo.s("a.txt");
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            // a revision the repository does not have
            let err = c
                .error(
                    "review/set",
                    json!({ "id": "x", "base": { "rev": "0123456789abcdef0123456789abcdef01234567" }, "files": files }),
                )
                .await?;
            assert_eq!(err["code"], 1007);
            assert_eq!(err["data"]["reason"], "unknown revision");
            // only a full commit id
            for rev in ["main", "HEAD~1", "0123456", ""] {
                let err = c
                    .error(
                        "review/set",
                        json!({ "id": "x", "base": { "rev": rev }, "files": files }),
                    )
                    .await?;
                assert_eq!(err["code"], -32602, "{rev}");
                assert_eq!(err["data"]["field"], "base");
                assert!(
                    err["message"].as_str().unwrap().contains("needs a full commit id"),
                    "{err}"
                );
            }
            let err = c.error("review/set", json!({ "id": "", "files": [] })).await?;
            assert_eq!(err["data"]["field"], "id");
            // a file outside any repository
            let out = tempfile::tempdir()?;
            let err = c
                .error(
                    "review/set",
                    json!({ "id": "x", "files": [{ "path": out.path().join("f").to_string_lossy(), "status": "added" }] }),
                )
                .await?;
            assert_eq!(err["data"]["reason"], "not a git repository");

            // :bb-review <target> asks the client
            c.subscribe(&["review", "send"]).await?;
            keys.send(":bb-review branch<ret>")?;
            let ev = c.event("event/review").await?;
            assert_eq!(ev["target"], "branch");
            assert_eq!(ev["path"], a_s.as_str());

            // event/send carries the review
            keys.send(":bb-comment no review yet<ret>")?;
            let ev = c.event("event/send").await?;
            assert!(ev.get("reviewId").is_none());
            c.call("review/set", json!({ "id": "rv", "files": [{ "path": a_s, "status": "modified" }] }))
                .await?;
            keys.send(":bb-comment in a review<ret>")?;
            let ev = c.event("event/send").await?;
            assert_eq!(ev["reviewId"], "rv");

            // :bb-diff toggles the inline diff
            keys.send(":bb-diff<ret>")?;
            until_status("Inline diff off").await?;
            assert_eq!(c.call("review/state", json!({})).await?["inline"], false);
            // the same id again keeps what the user chose
            c.call("review/set", json!({ "id": "rv", "files": [{ "path": a_s, "status": "modified" }] }))
                .await?;
            assert_eq!(c.call("review/state", json!({})).await?["inline"], false);
            keys.send(":bb-diff<ret>")?;
            until_status("Inline diff on (rv)").await?;

            // off ends it at once and tells the client
            keys.send(":bb-review off<ret>")?;
            let ev = c.event("event/review").await?;
            assert_eq!(ev["target"], "off");
            settle().await;
            assert_eq!(c.call("review/state", json!({})).await?["active"], false);
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn uncommitted_without_a_client_and_the_picker() -> anyhow::Result<()> {
    let repo = Repo::new();
    let a = repo.path("a.txt");
    let gone = repo.path("gone.txt");
    let mut bridge = Bridge::start(AppBuilder::new().with_file(&a, None))?;
    bridge
        .run(|keys, _| async move {
            keys.send(":bb-review branch<ret>")?;
            until_status("'bb-review': :bb-review branch needs bb (no client is listening)")
                .await?;
            keys.send(":bb-review uncommitted<ret>")?;
            for _ in 0..100 {
                if helix_term::bridge::status()
                    .await
                    .is_some_and(|s| s.starts_with("Review: uncommitted changes"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            settle().await;
            keys.send(":bb-review<ret>")?;
            until_overlay("picker").await?;
            Ok(())
        })
        .await?;
    // the uncommitted review compares with HEAD
    let head = git(&repo.root, &["show", "HEAD:a.txt"]) + "\n";
    assert_eq!(
        base_of(&bridge.app.editor, &a).as_deref(),
        Some(head.as_str())
    );
    // the files are sorted by path: added, a, gone, new; pick the deleted one
    bridge
        .run(|keys, _| async move {
            keys.send("gone<ret>")?;
            settle().await;
            Ok(())
        })
        .await?;
    {
        let doc = doc!(bridge.app.editor);
        // no path: `:w` cannot write a file named after it
        assert_eq!(doc.path(), None);
        let (status, _) = bridge
            .app
            .editor
            .get_status()
            .expect("named on the status line");
        assert!(status.contains("gone.txt (deleted)"), "{status}");
        assert!(doc.readonly);
        assert_eq!(doc.text().to_string(), "deleted one\ndeleted two\n");
        assert!(!doc.is_modified());
    }
    // `:w` refuses: there is no file name to write
    bridge
        .run(|keys, _| async move {
            keys.send(":w<ret>")?;
            until_status("Error saving: Can't save with no path set!").await
        })
        .await?;
    assert!(!gone.exists(), "nothing written");
    // Enter on a changed file opens it at its first change
    bridge
        .run(|keys, _| async move {
            keys.send(":bb-review<ret>")?;
            until_overlay("picker").await?;
            keys.send("a.txt<ret>")?;
            settle().await;
            Ok(())
        })
        .await?;
    {
        let editor = &bridge.app.editor;
        let doc = doc!(editor);
        assert_eq!(doc.path(), Some(a.as_path()));
        let text = doc.text().slice(..);
        let line = text.char_to_line(doc.selection(view!(editor).id).primary().cursor(text));
        // against HEAD the only change is the added last line
        assert_eq!(line, 19);
    }
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn review_open_waits_while_the_user_types() -> anyhow::Result<()> {
    let repo = Repo::new();
    let a = repo.path("a.txt");
    let mut bridge = Bridge::start(AppBuilder::new().with_file(&a, None))?;
    let files = repo.files();
    bridge
        .run(move |keys, socket| async move {
            let mut c = Client::connect(&socket).await?;
            keys.send("i")?;
            settle().await;
            let r = c
                .call(
                    "review/set",
                    json!({ "id": "r", "files": files, "open": true }),
                )
                .await?;
            assert_eq!(r["deferred"], true);
            keys.send("<esc>")?;
            keys.send(":bb-go<ret>")?;
            until_overlay("picker").await?;
            keys.send("<esc>")?;
            let r = c
                .call(
                    "review/set",
                    json!({ "id": "r", "files": [], "open": true, "interrupt": "always" }),
                )
                .await?;
            assert_eq!(r["deferred"], false);
            Ok(())
        })
        .await?;
    // an empty list cannot be picked from: the jump's error is on the status line
    let status = bridge.app.editor.get_status().map(|(m, _)| m.to_string());
    assert!(status.is_some_and(|s| s.contains("no files")));
    bridge.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn another_repository_and_files_outside_keep_head() -> anyhow::Result<()> {
    let repo = Repo::new();
    let other = Repo::new();
    let outside = tempfile::tempdir()?;
    let loose = outside.path().join("loose.txt");
    std::fs::write(&loose, "not in git\n")?;
    let a = repo.path("a.txt");
    let other_a = other.path("a.txt");
    let mut bridge = Bridge::start(AppBuilder::new().with_file(&a, None))?;
    let (files, base) = (repo.files(), repo.base.clone());
    let (o, l) = (other_a.clone(), loose.clone());
    bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.call(
                "review/set",
                json!({ "id": "r", "base": { "rev": base }, "files": files }),
            )
            .await?;
            for path in [&o, &l] {
                c.call(
                    "editor/open",
                    json!({ "path": path, "interrupt": "always" }),
                )
                .await?;
            }
            settle().await;
            Ok(())
        })
        .await?;
    let editor = &bridge.app.editor;
    let at_base: String = (1..=20).map(|i| format!("a line {i}\n")).collect();
    assert_eq!(base_of(editor, &a).as_deref(), Some(at_base.as_str()));
    // the other repository's file still compares with its own HEAD
    let head = git(&other.root, &["show", "HEAD:a.txt"]) + "\n";
    assert_eq!(base_of(editor, &other_a).as_deref(), Some(head.as_str()));
    // a file in no repository has no diff
    let real = std::fs::canonicalize(&loose)?;
    let loose = if editor.document_by_path(&loose).is_some() {
        loose
    } else {
        real
    };
    assert!(editor.document_by_path(&loose).is_some());
    assert_eq!(base_of(editor, &loose), None);
    bridge.finish().await?;
    Ok(())
}

/// The normal case: an agent edits a file while the owner reviews it. The
/// watch reloads it, the base stays the review's (not `HEAD`), the reveal
/// and the inline diff are both on, and `event/reloaded` goes out.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn a_watched_reload_during_a_review_keeps_its_base() -> anyhow::Result<()> {
    let repo = Repo::new();
    let a = repo.path("a.txt");
    let mut bridge = Bridge::start(AppBuilder::new().with_file(&a, None))?;
    let (files, base, a_s) = (repo.files(), repo.base.clone(), repo.s("a.txt"));
    let now = std::fs::read_to_string(&a)?;
    let edited = now.replace("a line 3\n", "a line 3\nthe agent wrote this\n");
    let edited2 = edited.clone();
    let reloaded = bridge
        .run(move |_, socket| async move {
            let mut c = Client::connect(&socket).await?;
            c.subscribe(&["documents"]).await?;
            c.call(
                "review/set",
                json!({ "id": "r", "title": "Branch", "base": { "rev": base }, "files": files }),
            )
            .await?;
            settle().await;
            // written the way editors and agents write: a new file renamed over
            let tmp = format!("{a_s}.tmp~");
            std::fs::write(&tmp, &edited2)?;
            std::fs::rename(&tmp, &a_s)?;
            let ev = c.event("event/reloaded").await?;
            settle().await;
            let t = c.call("editor/text", json!({ "path": a_s })).await?;
            assert_eq!(t["text"], edited2.as_str());
            Ok(ev)
        })
        .await?;
    assert_eq!(reloaded["origin"], "watch");
    assert_eq!(reloaded["ranges"][0]["start"]["line"], 4);
    let at_base: String = (1..=20).map(|i| format!("a line {i}\n")).collect();
    assert_eq!(
        base_of(&bridge.app.editor, &a).as_deref(),
        Some(at_base.as_str()),
        "still the review's base after the reload"
    );
    // line 5 changed and line 10 removed since the base, a line added at the
    // end, and the agent's line: four hunks against the base
    assert_eq!(hunks_of(&bridge.app.editor, &a).len(), 4);
    assert!(inline_diff());
    let doc = doc!(bridge.app.editor);
    assert_eq!(doc.text().to_string(), edited);
    assert!(doc.bridge_annotations.reveal.is_some(), "the reveal plays");
    // a frame with both drawn (the harness renders after every event)
    bridge
        .run(|keys, _| async move {
            keys.send("j")?;
            settle().await;
            Ok(())
        })
        .await?;
    bridge.finish().await?;
    Ok(())
}
