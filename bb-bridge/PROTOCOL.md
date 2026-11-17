# Helix bridge protocol, version 1

The bridge lets one or more local clients (the bb plugin, a CLI, tests) talk to
a running Helix. Helix is the server. This file is the contract: the Rust side
and the plugin side are built against it independently, so change it here
first, bump nothing silently, and keep it exact.

`PROTOCOL_NOTES.md` beside this file records how the implementation reads the
points this document leaves open (for example: selection events are throttled
with a leading edge; `languageId` is Helix's language name; `viewId` is null
for a background open; a `%` in an inline command note is Helix expansion
syntax). Those notes are part of the contract.

## Transport

- One Unix domain socket per Helix process:
  `<cache_dir>/bridge/<pid>.sock` where `cache_dir` is Helix's cache directory
  (`helix_loader::cache_dir()`, normally `~/.cache/helix`). The directory is
  created `0700`, the socket `0600`.
- Next to it, a registry file `<pid>.json`, written atomically after the
  socket is listening and removed on clean exit:

  ```json
  {
    "protocol": 1,
    "pid": 4242,
    "socket": "/Users/me/.cache/helix/bridge/4242.sock",
    "helixVersion": "25.07.1 (abc1234)",
    "startedAt": "2026-11-16T22:10:03Z",
    "cwd": "/repo/services/payments",
    "workspaceRoot": "/repo",
    "env": { "BB_THREAD_ID": "thr_…", "BB_ENVIRONMENT_ID": "env_…", "BB_PROJECT_ID": "proj_…", "BB_TERMINAL_ID": "…", "BB_TERMINAL_SESSION_ID": "…" }
  }
  ```

  `env` carries only those five variables, and only the ones that are set.
  bb's own thread terminals export only `BB_TERMINAL_SESSION_ID`, which a
  client maps to the terminal's thread.
  A client that fails to connect to a listed socket may delete both files
  (stale instance).
- macOS limits a socket path to 104 bytes. When the default path would be
  longer than 100 bytes, Helix puts the socket at
  `/tmp/helix-bridge-<uid>/<pid>.sock` instead (directory `0700`, owned by the
  user, refused if it exists with another owner or mode). The registry file
  stays in the cache directory, and its `socket` field is always the path to
  connect to; clients never compute the socket path themselves.
- Framing: newline-delimited JSON, UTF-8, one message per line, no embedded
  newlines. Messages are JSON-RPC 2.0 objects without batching.
- Several clients may connect at once. Each has its own subscriptions.
- A line longer than 8 MiB closes the connection.

## Conventions

- **Paths** are absolute in everything Helix sends. Requests accept absolute
  paths or paths relative to `workspaceRoot`.
- **Positions** are `{ "line": L, "col": C }`, both 1-based. `col` counts
  Unicode scalar values (Rust `char`s) from the start of the line.
- **Ranges** are `{ "start": P, "end": P }`, end-exclusive. A cursor with no
  extent is a range whose start equals its end. Because the end is exclusive,
  a selection of whole lines 10 to 12 is `{10,1}` to `{13,1}`: a range that
  ends at column 1 of line N covers lines up to N-1. Anything that turns a
  range into "lines A to B" for a person or an agent must apply that rule.
- **Document version** is Helix's own integer revision counter for a buffer.
  It increases on every change, including undo.
- **Limits**: a response carries at most 1 MiB of document text and at most
  500 list items. Truncated results set `"truncated": true`; list methods take
  `limit` (1–500, default 100) and `offset`.
- **Errors** use JSON-RPC `error` with these codes:

  | Code | Name | Meaning |
  |---|---|---|
  | -32700 | parse_error | not valid JSON |
  | -32600 | invalid_request | not a request object |
  | -32601 | method_not_found | unknown method |
  | -32602 | invalid_params | missing or wrong params; `data.field` names it |
  | 1001 | not_found | file or document does not exist |
  | 1002 | version_mismatch | the buffer changed; `data.version` is current |
  | 1003 | buffer_modified | refused because the buffer has unsaved changes |
  | 1004 | no_language_server | no running server supports this for the file |
  | 1005 | timeout | the editor or language server did not answer in time |
  | 1006 | too_large | result exceeds a limit that cannot be paged |
  | 1007 | unsupported | valid method, not possible in this state |
  | 1008 | cancelled | the request was cancelled by `$/cancel` |

- A client cancels a request with the notification
  `{"jsonrpc":"2.0","method":"$/cancel","params":{"id":<id>}}`.
- Requests that touch the editor are answered within 2 s or fail with
  `timeout`; `lsp/*` requests take `timeoutMs` (default 10000, max 60000).

## Methods

### bridge/hello

Params: `{ "client": "bb-plugin", "clientVersion": "0.1.0" }`

Result: `{ "protocol": 1, "helixVersion", "pid", "cwd", "workspaceRoot",
"capabilities": ["editor", "reload", "locations", "views", "command",
"diagnostics", "lsp", "applyEdits", "annotations", "reveal"], "commandPolicy": "safe" }`. A capability is listed only when implemented;
clients feature-detect on it.

### editor/state

Params: `{}`. Result:

```json
{
  "mode": "normal",
  "focused": {
    "docId": 3, "path": "/repo/a.go", "languageId": "go", "version": 17,
    "modified": false,
    "selections": [{ "start": {"line": 42, "col": 5}, "end": {"line": 43, "col": 1} }],
    "primary": 0,
    "visible": { "firstLine": 30, "lastLine": 71 }
  },
  "documents": [{ "docId": 3, "path": "/repo/a.go", "languageId": "go", "version": 17, "modified": false }]
}
```

`mode` is `normal`, `insert` or `select`. `focused` is always present. For a
scratch buffer that has no file, `focused.path` is null and the other fields
are still set. `documents` lists every open buffer (path null for scratch buffers).

### editor/open

Params: `{ "path", "line"?, "col"?, "endLine"?, "endCol"?, "split"?: "none" |
"horizontal" | "vertical", "viewId"?, "focus"?: true, "interrupt"?:
"whenIdle" | "always", "note"? }`. Opens the file (or switches to it) in the
focused pane, a new split, or the given pane; selects the range or puts the
cursor at the position; centres it; shows `note` on the status line. With
`"focus": false` the file is loaded in the background without changing what
the user sees. Result: `{ "docId", "path", "version", "viewId",
"deferred": false }`. Errors: `not_found`.

**Not yanking the user.** Any request that moves the user's focus
(`editor/open` with focus, `locations/set` with `goto`, `view/focus`,
`editor/select`, `editor/command`) honours `interrupt`. With `whenIdle` (the
default), if the user is in insert mode or pressed a key in the last 1500 ms,
Helix does not move: it stores the request as the pending jump (one slot, the
newest wins), shows "bb wants to show <file>:<line> (:bb-go)" on the status
line, and answers `"deferred": true`. `:bb-go` performs it. `always` moves at
once. Every one of those methods accepts `interrupt` and includes
`"deferred": <bool>` in its result; a deferred `editor/command` has not run.

### editor/text

Params: `{ "path" | "docId", "range"? }`. Returns the buffer's current text,
unsaved changes included: `{ "path", "version", "modified", "text",
"truncated" }`. If the file is not open, Helix reads it from disk without
opening a buffer and reports `"version": null`.

### editor/selection

Params: `{}`. Result: `{ "path", "docId", "languageId", "version", "modified",
"primary", "ranges": [{ "start", "end", "text" }], "truncated" }` for the
focused view. When every range is a bare cursor, `text` is that line.

### editor/reload

Params: `{ "paths"?: [..], "force"?: false, "reveal"?: false, "follow"?:
false, "interrupt"? }`. Reloads open buffers whose file
changed on disk since Helix last read or wrote it. With `reveal`, the text
that changed is shown with the reveal animation (see `editor/reveal`); with
`follow`, the view also moves to the first change of the first reloaded
buffer, subject to `interrupt`. Without `paths`, considers
every open buffer. Buffers with unsaved changes are skipped unless `force`.
Selections and scroll position are preserved as far as the new text allows.
Result: `{ "reloaded": [path], "unchanged": [path], "skippedModified": [path],
"missing": [path] }`.

### editor/reveal

Params: `{ "path", "ranges": [Range], "removed"?: [{ "line", "text" }],
"durationMs"?: 600, "follow"?: false, "interrupt"? }`.

Shows text that an agent has just written as if it were being typed: the
buffer already holds the final text (so the file, the language server and
undo are never out of step), and for a short time the given ranges are
uncovered progressively, in order, with a highlight that fades. `removed`
lines are shown struck through above their position for the same time. It is
purely visual: nothing in the document changes. Any key press ends the
animation at once and shows the final state. Ranges outside the visible area
are not animated unless `follow` moves the view there. `durationMs` is the
total for the request (50 to 3000), whatever the amount of text. Capability
`reveal`. Result: `{ "animated": <number of ranges> }`.

`[editor.bridge] reveal = false` turns the animation off; requests then
succeed and do nothing.

### editor/applyEdits

Params: `{ "path", "version"?, "edits": [{ "range", "newText" }], "label"? ,
"save"?: false }`. Applies all edits as one undoable transaction (ranges refer
to the text before any edit; they must not overlap). The file is opened in the
background if it is not open. With `version`, fails with `version_mismatch`
when the buffer moved on. Result: `{ "path", "version", "modified" }`.

### editor/status

Params: `{ "message", "severity"?: "info" | "warning" | "error" }`. Shows a
status-line message. Result: `{}`.

### locations/set

Params: `{ "list", "title", "items": [{ "path", "line", "col"?, "endLine"?,
"label"?, "note"? }], "activate"?: true, "goto"?: 0 }`.

Replaces a named list of places the user can walk with `:bb-next` and
`:bb-prev` and browse with `:bb-locations` (see Commands). Two list names are
conventional: `changes` (what an agent just edited) and `tour` (places an
agent wants to show, in order, each with a `note` explaining it). Any other
name is allowed. `activate` makes it the list the walk commands use; `goto`
also jumps to that item (subject to `interrupt`, see `editor/open`). While
the user stands on an item, its `note` shows on the status line. At most 2000
items. Result: `{ "count" }`. `locations/clear` takes `{ "list"? }`.

### view/list

Params: `{}`. Result: `{ "views": [{ "viewId", "docId", "path", "focused",
"area": { "x", "y", "width", "height" }, "firstLine", "lastLine" }],
"layout": Node }` where `Node` is `{ "split": "horizontal" | "vertical",
"children": [Node] }` or `{ "viewId" }`. A view is one pane.

### view/focus, view/close

Params: `{ "viewId" }`. `view/close` closes the pane, not the buffer, and
refuses to close the last one (`unsupported`). Result: `{}`.

### editor/select

Params: `{ "viewId"?, "ranges": [Range], "primary"?: 0, "center"?: true }`.
Sets the selections of a view (default: the focused one) and scrolls them
into view. Result: `{ "version" }`.

### editor/command

The general control method: anything the user can do from the keyboard.
Params, exactly one of:

- `{ "typed": "vsplit src/main.go" }`: a typed command line, without the
  leading colon.
- `{ "static": "goto_definition", "count"?: 1 }`: a named editor command, the
  names Helix's key maps use.
- `{ "keys": "<space>f" }`: key events in Helix's own key notation, handled
  as if typed.

Result: `{ "status"?: { "message", "severity" } }` with whatever the command
put on the status line. Subject to the command policy below; a refused
command fails with `unsupported` and `data.reason`.

**Command policy** (`[editor.bridge] commands`, enforced in Helix):

| Value | Allows |
|---|---|
| `"safe"` (default) | every command except those that run a shell (`sh`, `run-shell-command`, `pipe`, `pipe-to`, `insert-output`, `append-output`, and their static equivalents), shell expansions in typed lines, and the force variants that discard unsaved work (`quit!`, `quit-all!`, `buffer-close!`, `buffer-close-all!`, `buffer-close-others!`, `reload!` on a modified buffer, `cquit!`) |
| `"all"` | everything |
| `"none"` | `editor/command` is disabled |

`keys` are checked after key-map resolution, command by command, under the
same policy; a sequence that opens the command prompt is allowed only up to
what the policy would allow as `typed`.

### annotations/set

Params: `{ "namespace", "path", "items": [{ "line", "endLine"?, "text",
"kind"?: "comment" | "reply" | "note", "severity"?: "info" | "warning" |
"error" | "hint", "sign"?: "<one grapheme>", "highlight"?: false }] }`.

Replaces what this namespace shows in that file. Each item marks lines `line`
to `endLine` (default: `line`) with a sign in the gutter, optionally a subtle
background on those lines (`highlight`), and its `text` as virtual lines
directly under `endLine`, wrapped to the view width and indented to the code,
styled by `kind` and `severity` from the theme's own scopes so it looks right
in any theme. Several items on the same line stack in order. Virtual lines
are not part of the document: they are never selected, yanked, saved or sent
to the language server, and they move with their lines as the user edits. An
empty `items` clears the namespace for that file; `annotations/clear` takes
`{ "namespace", "path"? }`. At most 500 items per file and 2000 characters
per item. Result: `{ "count" }`.

`:bb-annotations` toggles all bridge annotations on and off for the session.

### diagnostics/get

Params: `{ "path"?, "minSeverity"?: "hint" | "info" | "warning" | "error",
"limit"?, "offset"? }`. Diagnostics Helix currently holds, for one file or the
whole workspace. Result: `{ "items": [{ "path", "range", "severity",
"message", "source"?, "code"?, "server"? }], "total", "truncated",
"counts": { "errors", "warnings", "infos", "hints" } }`, errors first, then by
path and position. `counts` covers everything in scope (the file, or the
workspace), before `minSeverity`, `limit` and `offset` are applied. A request
with `"limit": 0` returns only `counts` and `total`.

### lsp/servers

Params: `{}`. Result: `{ "servers": [{ "id", "name", "languages": [..],
"root", "initialized": true, "features": ["definition", "references", …] }] }`.

### lsp/query

One method for read-only language-server queries, answered by the server Helix
already runs. Params:

```json
{ "kind": "references", "path": "/repo/a.go", "position": {"line": 42, "col": 9},
  "query": "…", "includeDeclaration": true, "limit": 100, "timeoutMs": 10000,
  "ensure": true }
```

| `kind` | Needs | Result |
|---|---|---|
| `definition`, `typeDefinition`, `implementation`, `references` | `path`, `position` | `{ "locations": [Location], "total", "truncated" }` |
| `hover` | `path`, `position` | `{ "markdown" }` (empty string when none) |
| `documentSymbols` | `path` | `{ "symbols": [Symbol], … }` flattened, with `container` |
| `workspaceSymbols` | `query` | `{ "symbols": [Symbol], … }` |
| `incomingCalls`, `outgoingCalls` | `path`, `position` | `{ "calls": [{ "name", "kind", "location" }], … }` |
| `renamePreview` | `path`, `position`, `newName` | `{ "edits": [{ "path", "range", "newText" }], "files" }`, not applied |

`Location` is `{ "path", "range", "preview" }` where `preview` is the text of
the first line of the range, read from the open buffer or from disk, at most
200 characters. `Symbol` is `{ "name", "kind", "location", "container"? }`.

The server is chosen from the file's language. If the file is not open and
`ensure` is true (the default), Helix loads it in the background so the server
knows it. Errors: `no_language_server`, `timeout`, `not_found`.

### events/subscribe, events/unsubscribe

Params: `{ "events": ["selection", "documents", "diagnostics", "mode",
"send", "lsp", "externalChange"] }`. Result: `{ "subscribed": [..] }`.
Events arrive as notifications. With no subscriber for a kind, Helix does no
work for it.

| Notification | Params | Notes |
|---|---|---|
| `event/selection` | `{ "path", "docId", "version", "primary", "ranges": [Range] }` | focused view only; coalesced, at most one per 50 ms, no text |
| `event/documentOpened` / `event/documentClosed` | `{ "docId", "path", "languageId" }` | kind `documents` |
| `event/documentChanged` | `{ "docId", "path", "version", "modified" }` | coalesced per document, at most one per 100 ms |
| `event/documentSaved` | `{ "docId", "path", "version" }` | kind `documents` |
| `event/focus` | `{ "docId", "path" }` | kind `documents`; the focused buffer changed |
| `event/diagnostics` | `{ "path", "errors", "warnings", "infos", "hints" }` | coalesced per path, 200 ms |
| `event/mode` | `{ "mode" }` | |
| `event/send` | `{ "kind": "send" \| "ask" \| "comment" \| "flush", "note", "path", "docId", "languageId", "version", "modified", "ranges": [{ "start", "end", "text" }], "truncated", "sendNow": false }` | the user ran one of the `:bb-*` commands below; `flush` carries no ranges |
| `event/lsp` | `{ "id", "name", "state": "initialized" \| "exited" }` | |
| `event/externalChange` | `{ "path", "modified": true }` | a file open with unsaved changes also changed on disk |

A slow client never blocks the editor: each client has a bounded outbound
queue (1024 messages); when it overflows the oldest coalescable events are
dropped and the client receives `event/overflow` `{ "dropped": N }`.

## Commands added to Helix

| Command | Does |
|---|---|
| `:bb-send [note]` | Emits `event/send` (`kind: "send"`) with the current selections; `:bb-send!` sets `sendNow`. |
| `:bb-ask <question>` | Emits `event/send` with `kind: "ask"` and `sendNow: true`. |
| `:bb-comment [text]` | Emits `event/send` with `kind: "comment"`: a review comment on the selected lines, kept by the client until flushed. With no text it opens a one-line `comment:` prompt and emits on Enter; Escape cancels. |
| `:bb-comments-send` | Emits `event/send` with `kind: "flush"`: send every pending comment to the thread. |
| `:bb-locations [list]` | Opens a picker over a list set by `locations/set` (default: the active one) and makes it active. |
| `:bb-next`, `:bb-prev` | Jump through the active list. |
| `:bb-go` | Performs the pending jump a client requested while the user was busy. |
| `:bb-status` | Shows the socket path and connected clients. |

When no client is subscribed to `send`, these commands fail with a status-line
error and change nothing. After emitting, Helix shows a short status message
("Comment added", "Sent to bb"), which a client may replace with
`editor/status`.

Every command is an ordinary typed command with no default key binding, so the
user binds it in their own `config.toml` like any other, for example
`C = ":bb-comment"` under `[keys.select]`. The bridge never adds, removes or
overrides key bindings, and it reads the user's normal Helix configuration.

## Configuration

```toml
[editor.bridge]
enable = true          # default in this fork; false disables the socket entirely
commands = "safe"      # what editor/command may run: "safe", "all" or "none"
reveal = true          # animate text an agent just wrote (editor/reveal)
```

`HELIX_BRIDGE=0` in the environment also disables it.
