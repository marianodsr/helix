# Protocol notes (step 1)

Where the Helix side, as built, reads PROTOCOL.md in a particular way or
cannot do exactly what it says. Items marked **differs** need a decision
(change the contract or the code); the rest are interpretations a client
should know about.

## Positions, ranges, selections

- **Cursors** (**differs**, unavoidable): Helix has no zero-width selection.
  A cursor is a range one grapheme wide, so a range covering one grapheme
  (or none) is reported as a bare cursor, `start == end` at the cursor. A
  deliberate one-character selection is therefore also reported as a
  cursor. Wider ranges are `start`..`end`, end-exclusive, `start <= end`
  whatever the direction of the Helix selection.
- When every range is a cursor, each range's `text` (`editor/selection`,
  `event/send`) is the cursor's line **without** its line ending. Otherwise
  each range carries its own text; a line selected with `x` includes the
  newline.
- Positions in requests are clamped: a column past the end of a line goes
  to the end of that line (before the line ending), a line past the end of
  the file goes to the end of the text. `line`/`col` below 1 count as 1.
- `editor/open` with `endLine` but no `endCol` selects through the end of
  `endLine` (so `line: 10, endLine: 12` selects lines 10 to 12).

## Methods

- `languageId` is Helix's language name from `languages.toml` (`go`,
  `rust`, `tsx`, `starlark`, `protobuf`), not the LSP language id; `null`
  when the buffer has none.
- `bridge/hello`: `capabilities` is `["editor"]` in step 1. `cwd` is read
  when asked (follows `:cd`); the registry file's `cwd` and both
  `workspaceRoot`s are fixed at start. `helixVersion` is Helix's
  `VERSION_AND_GIT_HASH`, for example `25.7.1 (dc889dba)`.
- `editor/state`: `documents` is sorted by `docId`. If there are more than
  500, the list is cut and the result gains `"truncated": true` (absent
  otherwise).
- `editor/open`:
  - `viewId` is Helix's own view key as a number. It is stable for the life
    of the pane and usually above 2^32 (still exact in JavaScript).
  - Result `viewId` is the pane showing the file; it is `null` with
    `"focus": false`.
  - With `viewId` and focus, that pane is focused first and the file opens
    there (or, with `split`, in a new split of it). With `"focus": false`,
    `viewId` and `split` are ignored: the file is loaded in the background.
  - `interrupt` is validated (`whenIdle`, `always`, else `invalid_params`
    with `field: "interrupt"`) and otherwise ignored until step 2c: every
    request moves at once and `deferred` is always `false`.
  - `note` is shown with the normal status-line severity.
  - A directory is `not_found`. A path that does not exist and is not an
    open buffer is `not_found` (Helix would otherwise create a new file).
  - Without `line`, the cursor stays where Helix puts it and the view is
    not re-centred.
- `editor/text`: `docId` wins when both are given; an unknown `docId` is
  `not_found` (no disk fallback). Files read from disk are limited to
  64 MiB (`too_large`) and decoded as UTF-8 with invalid bytes replaced;
  they report `"version": null, "modified": false`.
- `events/subscribe` and `events/unsubscribe` return the client's whole
  subscription set after the change, in this order: `selection`,
  `documents`, `diagnostics`, `mode`, `send`, `lsp`, `externalChange`. An
  unknown kind fails the whole request (`invalid_params`,
  `field: "events"`) and changes nothing.

## Events

- **Coalescing is a throttle with a leading edge** (meets "at most one per
  50 ms / 100 ms"; worth knowing): the first change after a quiet window is
  sent at once; further changes inside the window become one event at the
  end of the window, with the final state. A lone cursor move is therefore
  reported within a few milliseconds instead of after 50 ms.
- `event/selection` is not repeated when nothing in it changed (same doc,
  version and ranges).
- A pending `event/documentChanged` is sent before `event/documentSaved` or
  `event/documentClosed` of the same document, even inside its window, so
  the order is always right.
- **`modified` in `event/documentChanged`** (**differs** slightly): Helix
  records an edit in the document only after its change event, so the hook
  cannot read the final value. Every change reports `"modified": true`. When
  undo, redo or reload bring the focused document back to its saved state,
  a second `event/documentChanged` with the same `version` and
  `"modified": false` follows, after the command (for typed commands like
  `:reload`, after the next key press). Buffers that are not focused are not
  corrected. `event/documentSaved` always means "not modified".
- `event/focus` is sent when the focused **document** changes, not when
  focus moves between two panes showing the same document. A focus change
  is followed by an `event/selection` for the newly focused pane.
- Focus and mode changes that happen outside a key binding, an open, a
  close or a pane switch (for example `:q` closing the focused split) are
  noticed at the next key press.
- `diagnostics`, `lsp` and `externalChange` are accepted by subscribe and
  emit nothing in step 1.

## Commands

- `event/send` fields: `note` is `""` when there is none. `send` has
  `sendNow: false` (`true` for `:bb-send!`), `ask` and `flush` have
  `sendNow: true`, `comment` has `false`. `flush` has `ranges: []` and
  `truncated: false` but still carries the focused document's `path`,
  `docId`, `languageId`, `version`, `modified`.
- In a note typed after the command (`:bb-send 50%% done`), `%` starts a
  Helix expansion (`%{buffer_name}`), so a literal `%` is written `%%`.
  Quotes are taken literally. Text typed into the `:bb-comment` prompt is
  taken literally, `%` included.
- `:bb-comment` opens its prompt through Helix's job queue, as every typed
  command that opens a prompt or picker does. Keys arriving in the same
  instant (a paste right after the binding) can reach the editor before the
  prompt.
- `:bb-ask` with no question fails with a status-line error. `:bb-comment`
  with an empty prompt sends nothing.

## Transport

- Requests run concurrently and may be answered out of order. A client may
  have 64 requests in flight; beyond that the bridge stops reading from it
  until one finishes.
- `$/cancel`: a cancelled request is answered with `cancelled` (1008). If it
  already reached the editor it may still take effect; the same holds for a
  request that times out. `$/cancel` sent as a request (with an `id`) is
  answered with `{}`.
- Outbound queue: 1024 messages per client, responses and events together.
  When it is full the oldest coalescable event (`event/selection`,
  `event/documentChanged`; later `event/diagnostics`) is dropped; if there
  is none, the client has not read anything at all and is disconnected.
  `event/overflow { dropped }` goes out ahead of the next batch the client
  accepts, counting drops since the previous overflow.
- A line over 8 MiB closes the connection without an error response.
- JSON-RPC details: an `id` that is not a number or string gets
  `invalid_request` with `id: null`; missing `"jsonrpc": "2.0"` is
  `invalid_request`; responses sent by a client are ignored; unknown
  notifications are ignored; `params` must be an object, `null` or absent
  (`invalid_params`, `field: "params"`); unknown fields in params are
  ignored.

## Lifecycle and configuration

- The registry file is written `0600`, without fsync (it only matters while
  the process runs; the rename makes it atomic).
- `enable`, `commands` and `HELIX_BRIDGE` are read at start;
  `:config-reload` does not start or stop the bridge.
- `[editor.bridge] commands` accepts `safe`, `all`, `none` and is returned
  as `commandPolicy`; `editor/command` itself is step 2c.
- A socket path over 100 bytes moves to `/tmp/helix-bridge-<uid>/<pid>.sock`
  as PROTOCOL.md says. If that directory exists but is not a directory
  owned by the user with mode `0700`, or any directory cannot be created,
  the bridge logs an error and stays off; Helix runs normally.
- A process killed without cleanup (SIGKILL, SIGHUP from a closed terminal)
  leaves both files behind; the next Helix with the same pid replaces them,
  and clients delete them when the socket refuses connections.
- Not available on Windows (no Unix sockets): the bridge and its `:bb-*`
  commands are left out there.

## Annotations (step 2d)

- **A file that is not open** keeps its annotations in Helix until it is
  opened, then shows them (positions as they are when it opens). A path that
  is neither open nor a file on disk is `not_found`. `count` is the number of
  items stored.
- `line` and `endLine` are clamped to the document; an `endLine` before
  `line` counts as `line`. A line past the end goes to the last line.
- `text`: tabs become four spaces, CRLF becomes LF, other control characters
  are dropped, trailing whitespace is trimmed. Over 2000 characters is
  `invalid_params` (`field: "text"`), over 500 items `field: "items"`; an
  empty `namespace` is `field: "namespace"`; a `sign` that is not exactly one
  grapheme is `field: "sign"`. An annotation longer than 24 rows is cut, with
  an ellipsis.
- **Where it shows**: as virtual lines under `endLine`, after Helix's own
  inline diagnostics for that line, indented to the code (the leading
  whitespace of the first non-blank marked line). In a narrow view the
  indent gives way so the text keeps at least 24 columns. Each view wraps to
  its own width.
- **Gutter**: every marked line gets `┃` in the item's colour; `sign`
  replaces it on the first marked line. It goes in the spacer column right
  of the diagnostics column (Helix's default layout), else in the
  diagnostics column, else in any spacer column, else nowhere. One sign per
  line: the first item in stacking order.
- **Styles**, from scopes every theme has: `comment` items use `function`'s
  colour for the bar and gutter sign, `reply` items `string`'s (and start
  with `↳`), `note` items `keyword`'s with the text in the theme's `comment`
  style. `severity` replaces the colour with `hint`, `info`, `warning` or
  `error`. The rows have a background tinted with that colour (13% into
  `ui.background`); `highlight` tints the marked lines (9%). Themes whose
  background is not an RGB colour get no tint (and `ui.cursorline.secondary`
  for `highlight`).
- **Edits**: annotations follow their lines (Helix maps them through every
  change, reload included). An annotation whose lines are deleted, line
  endings included, is removed; emptying a line keeps it. Undo does not
  bring a removed annotation back; the client sets it again, as a language
  server does with diagnostics.
- **Last line without a final newline**: Helix reserves no virtual lines
  after it, so the text is drawn in the empty area below the end of the
  file, and is cut off when that line is at the bottom of the view.
- `annotations/clear` answers `{}`. Without `path` it clears the namespace in
  every open document and every file waiting to be opened.
- `:bb-annotations` hides and shows every bridge annotation in the process;
  it is not saved. Removed lines of a running reveal still show.

## Reveal (step 2d)

- Result: `{ "animated": <ranges with something on screen>, "deferred":
  false }`. `deferred` is always false until step 2c: `follow` moves the
  focused view at once (cursor to the start of the first range, centred,
  opening the file in the focused pane if needed).
- A file that is not open is `animated: 0` without `follow` (nothing of it is
  on screen) and is opened with `follow`. A path that is neither open nor
  on disk is `not_found`. `durationMs` outside 50..3000 is `invalid_params`
  (`field: "durationMs"`); `interrupt` is validated like `editor/open`'s.
- "On screen" is, for every view showing the document, its first line to
  first line plus the view height. Ranges are clipped to that; the rest of
  the text is simply there. A range whose end is before its start is
  swapped.
- Timing: the ranges uncover in order, grapheme by grapheme, evenly over
  `durationMs`; each uncovered grapheme's background fades over half of
  `durationMs` (120 to 400 ms). Leading indentation and line endings are
  never highlighted. The animation ends `durationMs` plus that fade after it
  started.
- Covered graphemes are drawn in the theme's background colour, so wrapping
  and positions never move. With a non-RGB background, single-width
  graphemes are drawn as a Braille blank (U+2800) instead and wide ones show
  at once. Whitespace is never covered; soft-wrap indicators and indent
  guides of covered lines show from the start.
- `removed`: `line` is the line of the current text the removed text was
  above; the rows show under the line before it (under line 1 when
  `line` is 1). Text with newlines is several rows. Rows are not wrapped
  (cut at the view width), in `diff.minus` with `crossed_out` when the
  theme's `markup.strikethrough` has it and `dim` otherwise, on a light
  `diff.minus` tint. They show for the whole animation.
- Any key press or paste ends every animation at once. An edit to a
  document ends its animation. A new reveal of the same document replaces
  the running one.
- `[editor.bridge] reveal` is read at each request (`:config-reload`
  applies it). Off: requests succeed with `animated: 0`.
# Protocol notes (step 2b: diagnostics and language servers)

## diagnostics/get

- **What is listed.** For an open buffer: the diagnostics Helix shows for it
  (mapped through unsaved edits), which means only those from servers
  attached to that document with the diagnostics feature. For a file that
  is not open: what servers published for it, converted with the server's
  position encoding against the file on disk. If that file cannot be read
  (deleted, over 64 MiB) the server's numbers are passed through, plus one.
- A diagnostic without a severity counts as an error.
- Order: errors first, then by path (byte order), then start position.
- `limit` above 500 is cut to 500 (not an error). `"limit": 0` returns
  exactly `{ "counts", "total" }`. `total` is after `minSeverity`; `counts`
  before, as PROTOCOL.md says.
- `path` that is not open, has no diagnostics and does not exist on disk is
  `not_found`; an existing file with none gives an empty list.
- `source`, `code` (number or string) and `server` (the server's name) are
  left out when unknown.

## event/diagnostics

- Sent when a server publishes for a path (open or not) and when Helix's
  diagnostics for an open buffer change, even if the counts stay the same,
  so a client waiting for "the server looked at this again" is not left
  waiting. Throttled per path like the other events: the first change after
  a quiet 200 ms goes out at once, the rest of a burst as one event at the
  end of the window, with the final counts.
- When a server exits, every path that had counts gets an event if they
  changed (usually to zero).
- Coalescable: dropped first when a client's queue is full.

## lsp/servers

- `id` is Helix's language server id as a number (a slotmap key, like
  `viewId`); `event/lsp` uses the same id.
- `root` is the server's first workspace folder (the root Helix started it
  in); `null` if it has none.
- `languages`: Helix language names whose configuration lists the server.
- `features` (empty until initialized), from the server's capabilities: the
  `lsp/query` kinds it can answer first (`incomingCalls` and `outgoingCalls`
  for call hierarchy, `renamePreview` for rename), then Helix's other
  feature names: `declaration`, `diagnostics`, `pullDiagnostics`,
  `completion`, `signatureHelp`, `codeAction`, `format`,
  `documentHighlight`, `inlayHints`, `documentLinks`, `documentColors`,
  `workspaceCommand`. Per-language `only-features`/`except-features` from
  `languages.toml` apply when a query picks its server, not here.
- Extra field: `positionEncoding` (`utf-8`, `utf-16`, `utf-32`; `null` until
  initialized).

## lsp/query

- **Which server**: the first server attached to the document (in
  `languages.toml` order) that is initialized, supports the feature and is
  not excluded from it, the way Helix picks one for rename, hover and the
  symbol pickers. Helix's own goto commands ask every such server and merge
  the answers; the bridge asks only the first.
- `workspaceSymbols` without `path`: the focused document's server, else
  the first running server that supports it.
- **`ensure: true`** (default), file not open: loaded in the background
  (`editor/open` with `focus: false`), so the server gets `didOpen`; it is
  closed again when the last query using it finishes, unless the user has
  it in a pane by then or it was modified. The bufferline therefore does
  not fill up with files an agent asked about. Clients subscribed to
  `documents` see `documentOpened`/`documentClosed` for it. Reading the
  file (and Helix's syntax parse of it) happens on the main thread, as for
  `editor/open` with `focus: false`.
- **`ensure: false`**, file not open: a running server for the file's
  language (by file name) is asked about the file as it is on disk; the
  position is converted against the disk text, off the main thread.
- While a server for the file is still starting, the query waits for it
  (within `timeoutMs`).
- Positions in requests are clamped as everywhere else.
- **Locations**: `references` and `implementation` are sorted by path and
  position, duplicates removed; `definition` and `typeDefinition` keep the
  server's order. A `LocationLink` gives its `targetSelectionRange` (the
  name). `preview` is that line without its line ending, at most 200 chars,
  `""` when the file cannot be read. Non-`file:` URIs are dropped.
- Every list result has `total` and `truncated` and honours `limit`
  (0 to 500, default 100) and `offset`.
- **Symbols**: `kind` is the LSP symbol kind in camel case (`function`,
  `method`, `struct`, `interface`, `enumMember`, `typeParameter`, …).
  `location` is the symbol's name (`selectionRange`). `container` is the
  parent symbol's name (nested answers) or the server's `containerName`
  (flat answers); absent at top level. Extra field `extent`: the range of
  the whole symbol, for `documentSymbols` answered in nested form (gopls,
  rust-analyzer), so an agent can read a function's body.
  `workspaceSymbols` answered without a range point at 1:1 of the file.
- **Calls**: `{ "name", "kind", "location", "callSites": [Location] }`.
  `location` is the other function's name; `callSites` are the call
  expressions (in the caller's file for `incomingCalls`, in the queried
  file for `outgoingCalls`), at most 50 per call. When the position
  prepares several items, the calls of all of them are listed.
- **renamePreview**: `files` is the sorted list of paths touched; `edits` is
  sorted by path and position and paged (with `total` and `truncated`).
  File operations (create, rename, delete) come in `fileOperations:
  [{ "kind", "path", "newPath"? }]` when the server sends any. Nothing is
  applied.
- **hover**: language blocks become fenced code, several parts are joined
  with a blank line, plain-text content is passed as is.
- **Errors**: `no_language_server` has `data: { "language", "kind" }` with
  Helix's language name (or `null` when the file has no language). A server
  that answers with an error (gopls refusing an invalid new name) gives
  `unsupported` with `data: { "server", "serverCode" }`. A server that exits
  during the query gives `no_language_server` with `data.server`. Helix's
  own request timeout (`[editor.lsp] timeout`, default 20 s) is `timeout`.
  `timeoutMs` is clamped to 1–60000.
- **Cancelling**: `$/cancel`, `timeoutMs` and a disconnecting client send
  `$/cancelRequest` for the request in flight. gopls answers it with
  `RequestCancelled`, which Helix writes to its log as an error line.
- Extra param `timing: true` adds `timing: { "issueMs", "serverMs",
  "shapeMs", "totalMs" }`: until the request was handed to Helix's client,
  until the answer came back, building the result, whole handler. For
  benchmarks.

## event/lsp

- `initialized` when Helix reports a server initialized. `exited` when
  Helix reports one exited, and also for servers stopped by
  `:lsp-restart` or `:lsp-stop`, which Helix drops without an exit event:
  those are reported when the next server initializes or at the next key
  press.
