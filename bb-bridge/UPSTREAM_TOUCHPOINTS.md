# Upstream touchpoints

Every edit to a file that exists upstream (base `49986e667`), with the lines
(in this branch) and the reason. Everything else is new: `helix-term/src/bridge/`,
`helix-view/src/annotations/bridge.rs`, `helix-vcs/src/git/review.rs`, `helix-term/tests/test/bridge*.rs`
and `bb-bridge/`. Regenerate the hunk list with:

```sh
git diff 49986e667 -U0 -- helix-term helix-view helix-lsp Cargo.lock \
    ':!helix-term/src/bridge' ':!helix-term/tests/test/bridge*.rs' \
    ':!helix-view/src/annotations/bridge.rs' ':!helix-vcs/src/git/review.rs' \
    | grep -E '^(\+\+\+|@@)'
```

| File | Lines | Change | Why |
|---|---|---|---|
| `helix-term/Cargo.toml` | 55 | tokio features `+ "net", "sync"` | Unix socket listener; `oneshot`/`Notify`. |
| `helix-term/Cargo.toml` | 88 | `slotmap.workspace = true` | `viewId` as a number (`ViewId` is a slotmap key); already a workspace dependency. |
| `Cargo.lock` | 1701 | `slotmap` under helix-term | Follows from the line above. |
| `helix-term/src/lib.rs` | 6–10 | `pub mod bridge;` (Unix), `bridge/unsupported.rs` elsewhere | Module declaration. |
| `helix-term/src/main.rs` | 146 | `let config = helix_term::bridge::kitty_keyboard_from_env(config);` before `Application::new` (integration step) | `HELIX_KITTY_KEYBOARD=enabled\|disabled` overrides `kitty-keyboard-protocol` at start. Inside bb, bb's terminal answers the capability query before the editor surface can, so `auto` never turns the protocol on; the plugin sets the variable on sessions it starts. The terminal reads the setting once, when it is set up, so this is the one place; `:config-reload` keeps working as before. |
| `helix-term/src/application.rs` | 247–248 | `crate::bridge::start(&editor);` in `Application::new` | Start after the editor and job queue exist; binding happens in a background task. |
| `helix-term/src/application.rs` | 340–342 | `if !crate::bridge::skip_render() { .. }` around the `render()` after a dispatched job (integration step) | The one deliberate change to the main loop (ARCHITECTURE.md, integration step 1). Helix draws a frame after every job, so each bridge request cost one (3.7 ms measured on a 20k-line Go file, release). A read-only request's job sets a flag; this skips that frame and nothing else. Anything that asked for a redraw (`request_redraw`) still gets it through Helix's own redraw timer; every other job renders as before. |
| `helix-term/src/application.rs` | 264–268 | `crate::bridge::hand_over_terminal(..)` first in `render` (`:bb-files`) | A full-screen file manager needs the terminal, which only the application owns. `:bb-files` records what to run; at the start of the next frame this restores the terminal exactly as suspend does (`Terminal::restore`), runs the program attached to it, claims it back as `SIGCONT` does (`claim`, `resize`, `clear`) and lets the frame draw everything. One relaxed atomic load per frame otherwise. |
| `helix-term/src/application.rs` | 1297–1305 | `event_stream` (Unix): the `termina::EventStream` is made inside `crate::bridge::pausable_input(..)` (`:bb-files`) | termina's stream keeps a thread reading the terminal for as long as it lives, so it would take the file manager's keys. The wrapper drops it while the program runs and makes a new one at the next poll; the filter and reader are unchanged. Main thread only (the stream is not `Send`). |
| `helix-term/src/application.rs` | 628 | `crate::bridge::document_saved(..)` in `handle_document_write` | There is no "document saved" event; this is the one place a completed save is known, after the saved revision is recorded. |
| `helix-term/src/application.rs` | 856 | `crate::bridge::diagnostics_published(&uri);` in the `publishDiagnostics` handler (step 2b) | `DiagnosticsDidChange` only fires for open documents; servers like gopls publish for the whole workspace, and `event/diagnostics` covers those paths too. One relaxed atomic load when nobody subscribed. |
| `helix-term/src/application.rs` | 1360 | `crate::bridge::stop();` in `close` | Remove socket and registry file on every clean exit (`:q`, SIGTERM, SIGINT all end here). |
| `helix-term/src/commands/typed.rs` | 42 | `const fn none()` -> `pub(crate) const fn none()` | Bridge typed commands live in the bridge module and need an empty completer. |
| `helix-term/src/commands/typed.rs` | 49 | `const fn positional()` -> `pub(crate) const fn positional()` (step 5) | `:bb-review` completes its target words (`uncommitted`, `branch`, …) through Helix's own completer table. |
| `helix-term/src/commands/typed.rs` | 4110 | `.chain(crate::bridge::typable_commands())` in `TYPABLE_COMMAND_MAP` | Makes `:bb-*` real typed commands: runnable, bindable from `config.toml` (`C = ":bb-comment"`), documented. |
| `helix-term/src/commands/typed.rs` | 4257–4260 | same chain in `complete_command_line` | `:bb-` completes on the command line. |
| `helix-term/src/commands.rs` | 3590 | same chain in `command_palette` | `:bb-*` listed in the command palette. |
| `helix-term/src/ui/mod.rs` | 2, 16 | `mod document` and `mod text_decorations` become `pub(crate)` (step 2d) | The bridge draws annotations as a `Decoration` with `TextRenderer`, the same mechanism inline diagnostics use. |
| `helix-term/src/ui/editor.rs` | 208 | `crate::bridge::decorate(..)` just before `render_document` in `render_view` (step 2d) | Draws gutter signs, line backgrounds, the annotations' rows and the reveal; after every other decoration, so its virtual lines stack in the order `text_annotations` reserved them. Returns at once when the document has none. |
| `helix-term/src/ui/editor.rs` | 1594 | `crate::bridge::focus_gained(context.editor);` in the `Event::FocusGained` arm (step 2a) | There is no editor event for the terminal regaining focus; this arm is where Helix already handles it (focus events reach the editor view under pickers and prompts, which ignore them). The bridge only copies ids there and stats files on a blocking thread. |
| `helix-term/tests/integration.rs` | 19–34, 37–39 | `mod bridge;`, `bridge_annotations`, `bridge_buffers`, `bridge_command`, `bridge_files`, `bridge_lsp`, `bridge_review` and the key-press benchmark module, each `#[cfg(unix)]`; `bridge_watch` `#[cfg(target_os = "macos")]` | The bridge's integration tests (step 1, 2d, 2a, 2c, `:bb-files`, 2b, the review, the watch) and `bb-bridge/bench/keypress.rs` run in Helix's own harness. |
| `helix-view/src/editor.rs` | 438–439 | `pub bridge: BridgeConfig` field in `Config` | `[editor.bridge]`; `Config` denies unknown fields, so the section must be declared here. |
| `helix-view/src/editor.rs` | 1221 | `bridge: BridgeConfig::default()` | Default of the field above. |
| `helix-view/src/editor.rs` | 2655–2697 (end of file) | `BridgeConfig`, `BridgeCommandPolicy` | The section's type: `enable` (default true), `commands` (`safe`/`all`/`none`), `reveal` (step 2d), `file-manager` (what `:bb-files` runs without a client), `watch` (default true, step 6), `inline-diff` (step 5). Appended at the end to stay out of upstream's way. |
| `helix-vcs/src/git.rs` | 22, 35–37 | `pub mod review;` and, first in `get_diff_base` after the path is resolved, `if let Some(base) = review::diff_base(&file, trust_full) { return base; }` (step 5) | "The file at the review's base" in the one place every diff base comes from: opening a file, `:reload`, the bridge's reloads. With no review, or a file outside the review's repository, it returns `None` after one `RwLock` read and a path prefix check, and Helix reads `HEAD` as before. The override lives in the new `git/review.rs`, a child module so it can use `open_repo` and `find_file_in_commit`. |
| `helix-vcs/src/lib.rs` | 14–15 | `#[cfg(feature = "git")] pub use git::review;` (step 5) | The bridge sets and clears the review's base and reads blobs for the picker. |
| `helix-view/src/tree.rs` | 725–742 (just before the tests) | `Tree::root()` and `Tree::container(id)`, read-only, in an `impl Tree` block of their own (step 2c) | `view/list` mirrors Helix's layout tree. Containers and their layout are private, and the tree cannot be rebuilt from pane areas: closing a pane can leave a container nested in one of the same direction, which looks the same on screen. |
| `helix-view/src/annotations.rs` | 1 | `pub mod bridge;` (step 2d) | Annotation and reveal data live next to the inline-diagnostics annotations, in a new file. |
| `helix-view/src/document.rs` | 237–238, 781 | `pub bridge_annotations` field on `Document` and its default (step 2d) | The text-annotation machinery borrows what it shows from the document (like inlay hints and diagnostics), so the data must live there. |
| `helix-view/src/document.rs` | 2581–2593 (just before the tests) | `impl Document { pub fn last_saved_time() }` (step 2a), `pub fn clear_diff_handle()` (step 5) | The mtime Helix last read or wrote is private. The bridge compares it with the file's mtime to find external changes without a file watcher, and to refuse `applyEdits` with `save` before editing when Helix's own save would refuse. A separate `impl` block, to stay out of upstream's way. `clear_diff_handle`: when a review ends, a file it compared with an empty base (new in the review, untracked at `HEAD`) must stop diffing, as `Document::reload` does when `HEAD` has no base; the field is private. |
| `helix-view/src/view.rs` | 524 | `annotations::bridge::add_text_annotations(..)` at the end of `View::text_annotations` (step 2d) | Reserves the annotations' virtual lines and adds the reveal overlays, so cursor motion, scrolling and mouse positions account for them. Last, so they stack under inline diagnostics. |
| `helix-lsp/src/client.rs` | 43–46, 290, 294–299 | thread-local `LAST_REQUEST_ID`, set in `next_request_id`; `pub fn Client::last_request_id()` (step 2b) | Helix never sends `$/cancelRequest` and does not expose request ids. Read right after a typed request method returns (same thread, nothing in between), it names that request, so `lsp/query` can cancel it on timeout or `$/cancel` while still using Helix's own methods. No behaviour change for Helix. |

Not touched: any event definition, any key map, any existing hook or
handler, `helix-term/src/handlers.rs`. The main loop has the one condition
above and nothing else.

## Fixes carried on top of upstream

Not bridge code: upstream bugs fixed here because the fork is used daily.
Each one says how to tell when upstream has it, and then the fix is dropped
(take upstream's side of the conflict).

### Go doc-comment query: slow cursor moves in Go files with many comments

| File | Lines | Change |
|---|---|---|
| `runtime/queries/go/highlights.scm` | 234–247 | Doc comments: the repeated `(comment)*` is captured too (`(comment)* @comment.block.documentation`); the `(var_spec)` and `(const_spec)` branches are removed. |

**Symptom.** On `net/http/server.go` (Go 1.25, 4,102 lines) every cursor
move took ~27.5 ms from key to output against ~2.3 ms in Homebrew `hx`
25.07, at any width and any position in the file
(`bb-bridge/bench/move_latency.py`). Not the bridge, and not Helix code:
the 25.07.1 binary with this query is just as slow.

**Cause.** Upstream `fdd624e80` "queries/go: Fix doc comment highlights"
(#15377, 2026-03-14; bisected) added
`(source_file (comment) @doc . (comment)* . [declarations])`. Every
`//` line is its own `comment` node, so tree-sitter starts a partial match
on each line of a comment run and splits each one 9 ways (`*` and the
alternation) on every following line: a doc comment of about 28 lines already holds
more than 256 partial matches, the cursor's match limit
(`TREE_SITTER_MATCH_LIMIT`), and from then on every new capture scans and
evicts. The pattern is rooted at `source_file`, which always intersects the
viewport, so the highlighter redoes this for every top-level comment run in
the whole file on every render (27 ms of the 27.5). The eviction also loses
highlights: at Helix's limit the query leaves 21,118 doc-comment lines in
730 of the 7,339 `.go` files under the Go source tree without their doc
highlight. On `server.go` 173 rendered rows lose it, 63 of them all comment
colour.

**Fix.** With the repeated comments captured as well, a match that
started on an earlier line of a run holds every capture of one that
started later, so tree-sitter drops the later one ("drop shorter state"):
8 partial matches per run instead of 9 per line. `(var_spec)` and
`(const_spec)` are never children of `source_file` and only added
splits. Per 40-line viewport query (tree-sitter 0.26.9, harness against
the fork's `go.dylib`): 27.1 ms → 1.7 ms, no match-limit overflow.
Highlighting is the original query's intended result in 7,333 of 7,338
files (the original run with no match limit; one timed out). The other 5
each have a single comment run of 250+ lines (`testing/testing.go`'s
package doc): still too long for the limit, as with the original from ~28
lines. `cargo xtask highlight-check --dump go` on `server.go` changes 162
spans, all `comment` → `comment.block.documentation`. A rendered-screen
diff of all of `server.go` (`screen_dump.py`, 71 screens) changes 173 of
4,118 rows, all from no or plain comment colour to doc colour.

| `server.go`, key to end of output, p50 | 53 cols | 100 cols | 140 cols |
|---|---|---|---|
| Homebrew `hx` 25.07 | ~2.1 ms | ~2.3 | ~2.6 |
| fork, upstream query | 27.5 | 27.5 | 27.6 |
| fork, this fix | 2.4–6.4 (median ~4.8) | 2.3–6.0 (~4.8) | 2.2–6.1 (~5.5) |

(8 interleaved runs of 80–100 moves each, load 3–6.) The rest of the
gap to 25.07 is the pattern's remaining ~1.7 ms per render. Without it
the same build answers in 0.3 ms, which is the pre-#15377 cost, but the
pre-#15377 patterns highlighted only the first doc-comment run of the
file per declaration kind.

**Drop it when** upstream's Go `highlights.scm` no longer has the
uncaptured `(comment)*` (or tree-sitter's capture cursor stops tracking a
state per line). Check: `python3 bb-bridge/bench/move_latency.py --check 10`
with upstream's query passes. Not reported upstream yet. #15619 ("A large
number of commented lines causes terrible lags") predates #15377 and is a
related but different case.

## Against the plugin-system branch

Checked against the tip of `steel-event-system` (mattwparas/helix,
`424add100`, 2026-10-22) by applying each hunk to that tree:

| Touchpoint | On steel |
|---|---|
| `lib.rs`, `application.rs` (3 hunks), `integration.rs` | Apply cleanly (application.rs with offsets). |
| `editor.rs` field and default | **Conflict, trivial**: steel adds `enable_steel` right before `workspace_trust` in both places. Keep both lines. The appended `BridgeConfig` applies cleanly. |
| `typed.rs` `none()` | **Drop the hunk**: steel already made it `pub const fn`. |
| `typed.rs` `TYPABLE_COMMAND_MAP` | **Conflict, trivial**: steel is on an older upstream (`Lazy` instead of `LazyLock`); the one-line `.chain(...)` goes in the same place. |
| `typed.rs` `complete_command_line`, `commands.rs` `command_palette` | **Conflict, small**: steel rewrites these same expressions to chain its scripting-engine commands. Add the bridge chain next to steel's. |
| `helix-term/Cargo.toml` | **Conflict, trivial**: different neighbouring lines (steel adds its features and dependencies); the tokio line itself is unchanged there. |
| `Cargo.lock` | Regenerate. |

The hunks added after step 1 (step 2a to 2d: `helix-lsp/src/client.rs`,
`ui/editor.rs`, `ui/mod.rs`, `view.rs`, `tree.rs`, `document.rs`,
`annotations.rs`; integration step: the `skip_render` condition in
`application.rs`, `main.rs`) were not checked against that branch.

None of the bridge's own files exist on that branch. A later port is a
rebase with four small manual merges, all in lines that list commands or
config fields.
