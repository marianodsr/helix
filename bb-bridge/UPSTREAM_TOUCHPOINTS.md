# Upstream touchpoints

Every edit to a file that exists upstream (base `49986e667`), with the lines
(in this branch) and the reason. Everything else is new: `helix-term/src/bridge/`,
`helix-view/src/annotations/bridge.rs`, `helix-term/tests/test/bridge*.rs`
and `bb-bridge/`. Regenerate the hunk list with:

```sh
git diff 49986e667 -U0 -- helix-term helix-view Cargo.lock \
    ':!helix-term/src/bridge' ':!helix-term/tests/test/bridge*.rs' \
    ':!helix-view/src/annotations/bridge.rs' | grep -E '^(\+\+\+|@@)'
```

| File | Lines | Change | Why |
|---|---|---|---|
| `helix-term/Cargo.toml` | 55 | tokio features `+ "net", "sync"` | Unix socket listener; `oneshot`/`Notify`. |
| `helix-term/Cargo.toml` | 88 | `slotmap.workspace = true` | `viewId` as a number (`ViewId` is a slotmap key); already a workspace dependency. |
| `Cargo.lock` | 1701 | `slotmap` under helix-term | Follows from the line above. |
| `helix-view/src/editor.rs` | 438–439 | `pub bridge: BridgeConfig` field in `Config` | `[editor.bridge]`; `Config` denies unknown fields, so the section must be declared here. |
| `helix-view/src/editor.rs` | 1221 | `bridge: BridgeConfig::default()` | Default of the field above. |
| `helix-view/src/editor.rs` | 2655–2682 (end of file) | `BridgeConfig`, `BridgeCommandPolicy` | The section's type: `enable` (default true), `commands` (`safe`/`all`/`none`). Appended at the end to stay out of upstream's way. |
| `helix-term/src/lib.rs` | 6–10 | `pub mod bridge;` (Unix), `bridge/unsupported.rs` elsewhere | Module declaration. |
| `helix-term/src/application.rs` | 247–248 | `crate::bridge::start(&editor);` in `Application::new` | Start after the editor and job queue exist; binding happens in a background task. |
| `helix-term/src/application.rs` | 621 | `crate::bridge::document_saved(..)` in `handle_document_write` | There is no "document saved" event; this is the one place a completed save is known, after the saved revision is recorded. |
| `helix-term/src/application.rs` | 1350 | `crate::bridge::stop();` in `close` | Remove socket and registry file on every clean exit (`:q`, SIGTERM, SIGINT all end here). |
| `helix-term/src/commands/typed.rs` | 42 | `const fn none()` -> `pub(crate) const fn none()` | Bridge typed commands live in the bridge module and need an empty completer. |
| `helix-term/src/commands/typed.rs` | 4110 | `.chain(crate::bridge::typable_commands())` in `TYPABLE_COMMAND_MAP` | Makes `:bb-*` real typed commands: runnable, bindable from `config.toml` (`C = ":bb-comment"`), documented. |
| `helix-term/src/commands/typed.rs` | 4257–4260 | same chain in `complete_command_line` | `:bb-` completes on the command line. |
| `helix-term/src/commands.rs` | 3590 | same chain in `command_palette` | `:bb-*` listed in the command palette. |
| `helix-term/tests/integration.rs` | 19–20, 23–25 | `mod bridge;` and the key-press benchmark module (Unix) | Bridge integration tests and `bb-bridge/bench/keypress.rs` run in Helix's own harness. |
| `helix-term/tests/integration.rs` | 21–22 | `mod bridge_command;` (Unix) | Step 2c integration tests (panes, `editor/command` and its policy, not yanking the user). |
| `helix-view/src/tree.rs` | 725–742 (just before the tests) | `Tree::root()` and `Tree::container(id)`, read-only, in an `impl Tree` block of their own | `view/list` mirrors Helix's layout tree. Containers and their layout are private, and the tree cannot be rebuilt from pane areas: closing a pane can leave a container nested in one of the same direction, which looks the same on screen. |
| `helix-view/src/annotations.rs` | 1 | `pub mod bridge;` | Step 2d: annotation and reveal data live next to the inline-diagnostics annotations, in a new file. |
| `helix-view/src/document.rs` | 237–238, 781 | `pub bridge_annotations` field on `Document` and its default | The text-annotation machinery borrows what it shows from the document (like inlay hints and diagnostics), so the data must live there. |
| `helix-view/src/view.rs` | 524 | `annotations::bridge::add_text_annotations(..)` at the end of `View::text_annotations` | Reserves the annotations' virtual lines and adds the reveal overlays, so cursor motion, scrolling and mouse positions account for them. Last, so they stack under inline diagnostics. |
| `helix-view/src/editor.rs` | 2664–2665, 2673 | `reveal` in `BridgeConfig` | `[editor.bridge] reveal`; the struct is the bridge's own (appended in step 1). |
| `helix-term/src/ui/mod.rs` | 2, 16 | `mod document` and `mod text_decorations` become `pub(crate)` | The bridge draws annotations as a `Decoration` with `TextRenderer`, the same mechanism inline diagnostics use. |
| `helix-term/src/ui/editor.rs` | 208 | `crate::bridge::decorate(..)` just before `render_document` in `render_view` | Draws gutter signs, line backgrounds, the annotations' rows and the reveal highlight; after every other decoration, so its virtual lines stack in the order `text_annotations` reserved them. Returns at once when the document has none. |
| `helix-term/tests/integration.rs` | 21–24 | `mod bridge_annotations;` (Unix) | Step 2d integration tests. |
| `helix-term/tests/integration.rs` | 21–22 | `mod bridge_buffers;` (Unix) | Step 2a integration tests (reload, focus check, locations, applyEdits). |
| `helix-term/src/ui/editor.rs` | 1593 | `crate::bridge::focus_gained(context.editor);` in the `Event::FocusGained` arm | There is no editor event for the terminal regaining focus; this arm is where Helix already handles it (focus events reach the editor view under pickers and prompts, which ignore them). The bridge only copies ids there and stats files on a blocking thread (step 2a focus check). |
| `helix-view/src/document.rs` | 2818–2824 (end of file) | `impl Document { pub fn last_saved_time() }` | The mtime Helix last read or wrote is private. The bridge compares it with the file's mtime to find external changes without a file watcher, and to refuse `applyEdits` with `save` before editing when Helix's own save would refuse. A separate `impl` block appended at the end, to stay out of upstream's way. |
| `helix-lsp/src/client.rs` | 43–46, 290, 294–299 | thread-local `LAST_REQUEST_ID`, set in `next_request_id`; `pub fn Client::last_request_id()` (step 2b) | Helix never sends `$/cancelRequest` and does not expose request ids. Read right after a typed request method returns (same thread, nothing in between), it names that request, so `lsp/query` can cancel it on timeout or `$/cancel` while still using Helix's own methods. No behaviour change for Helix. |
| `helix-term/src/application.rs` | 849 | `crate::bridge::diagnostics_published(&uri);` in the `publishDiagnostics` handler (step 2b) | `DiagnosticsDidChange` only fires for open documents; servers like gopls publish for the whole workspace, and `event/diagnostics` covers those paths too. One relaxed atomic load when nobody subscribed. (Shifts the `close` hunk above to 1351.) |
| `helix-term/tests/integration.rs` | 21–22 | `mod bridge_lsp;` (Unix, step 2b) | Diagnostics and `lsp/*` integration tests. |

Not touched: the main loop, any event definition, any key map, any
existing hook or handler, `helix-term/src/handlers.rs`.

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

The step 2b hunks (`helix-lsp/src/client.rs`, the `publishDiagnostics`
line in `application.rs`) were not checked against that branch.

None of the bridge's own files exist on that branch. A later port is a
rebase with four small manual merges, all in lines that list commands or
config fields.
