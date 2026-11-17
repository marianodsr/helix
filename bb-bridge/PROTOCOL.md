# Helix bridge protocol, version 1

The bridge lets one or more local clients (the bb plugin, a CLI, tests) talk to
a running Helix. Helix is the server. This file is the contract: the Rust side
and the plugin side are built against it independently, so change it here
first, bump nothing silently, and keep it exact.

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
    "env": { "BB_THREAD_ID": "thr_…", "BB_ENVIRONMENT_ID": "env_…", "BB_PROJECT_ID": "proj_…", "BB_TERMINAL_ID": "…" }
  }
  ```

  `env` carries only those four variables, and only the ones that are set.
  A client that fails to connect to a listed socket may delete both files
  (stale instance).
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
  extent is a range whose start equals its end.
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
"capabilities": ["editor", "reload", "changes", "diagnostics", "lsp",
"applyEdits", "annotations"] }`. A capability is listed only when implemented;
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

`mode` is `normal`, `insert` or `select`. `focused` is null for a scratch
buffer with no path only in its `path` field; `focused` itself is always
present. `documents` lists every open buffer (path null for scratch buffers).

### editor/open

Params: `{ "path", "line"?, "col"?, "endLine"?, "endCol"?, "split"?: "none" |
"horizontal" | "vertical", "focus"?: true }`. Opens the file (or switches to
it), selects the given range or puts the cursor at the position, and centres
it. With `"focus": false` the file is loaded in the background without
changing what the user sees. Result: `{ "docId", "path", "version" }`.
Errors: `not_found`.

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

Params: `{ "paths"?: [..], "force"?: false }`. Reloads open buffers whose file
changed on disk since Helix last read or wrote it. Without `paths`, considers
every open buffer. Buffers with unsaved changes are skipped unless `force`.
Selections and scroll position are preserved as far as the new text allows.
Result: `{ "reloaded": [path], "unchanged": [path], "skippedModified": [path],
"missing": [path] }`.

### editor/applyEdits

Params: `{ "path", "version"?, "edits": [{ "range", "newText" }], "label"? ,
"save"?: false }`. Applies all edits as one undoable transaction (ranges refer
to the text before any edit; they must not overlap). The file is opened in the
background if it is not open. With `version`, fails with `version_mismatch`
when the buffer moved on. Result: `{ "path", "version", "modified" }`.

### editor/status

Params: `{ "message", "severity"?: "info" | "warning" | "error" }`. Shows a
status-line message. Result: `{}`.

### changes/set

Params: `{ "id", "title", "items": [{ "path", "line", "endLine"?, "label"? }] }`.
Replaces the "changes" list the user can walk (see Commands). Used to follow
what an agent just edited. At most 2000 items. Result: `{ "count" }`.
`changes/clear` takes `{}`.

### annotations/set

Params: `{ "namespace", "path", "items": [{ "line", "text", "severity"?:
"info" | "warning" | "error" | "hint" }] }`. Replaces the virtual lines this
namespace shows under the given lines of that file. An empty `items` clears
them. Result: `{ "count" }`.

### diagnostics/get

Params: `{ "path"?, "minSeverity"?: "hint" | "info" | "warning" | "error",
"limit"?, "offset"? }`. Diagnostics Helix currently holds, for one file or the
whole workspace. Result: `{ "items": [{ "path", "range", "severity",
"message", "source"?, "code"?, "server"? }], "total", "truncated" }`, errors
first, then by path and position.

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
| `event/send` | `{ "kind": "send" \| "ask", "note", "path", "docId", "languageId", "version", "modified", "ranges": [{ "start", "end", "text" }], "truncated", "sendNow": false }` | the user ran `:bb-send` or `:bb-ask` |
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
| `:bb-changes` | Opens a picker over the list set by `changes/set`. |
| `:bb-next-change`, `:bb-prev-change` | Jump through that list. |
| `:bb-status` | Shows the socket path and connected clients. |

When no client is subscribed to `send`, `:bb-send` and `:bb-ask` fail with a
status-line error and change nothing.

## Configuration

```toml
[editor.bridge]
enable = true          # default in this fork; false disables the socket entirely
```

`HELIX_BRIDGE=0` in the environment also disables it.
