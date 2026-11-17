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
