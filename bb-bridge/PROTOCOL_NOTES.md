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
- `editor/open` with `endLine` but no `endCol` selects lines `line` to
  `endLine` whole, line ending included, as `x` selects them (so `line: 10,
  endLine: 12` selects lines 10 to 12 and the range ends at `{13, 1}`; on the
  last line of a file without a final newline it ends at the end of the
  text). Fixed in step 2c: before, a column past `usize::MAX - start`
  wrapped and lost the last line.

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

## Step 2c: panes, selections, `editor/command`, not yanking the user

### Panes

- `view/list`: `layout` mirrors Helix's own tree. The root is always a
  container, even around a single pane. `"vertical"` puts its children side
  by side (`:vsplit`), `"horizontal"` stacks them (`:hsplit`). After closes
  a container can sit directly inside one of the same direction (Helix does
  not flatten them), and the root can hold a single container. `views` is in
  layout order (left to right, top to bottom); `area` is in terminal cells
  and includes the pane's own status line; `firstLine`/`lastLine` are as in
  `editor/state` `visible`.
- `view/focus` and `view/close` answer `{ "deferred": <bool> }` (PROTOCOL.md
  says `{}`; every focus-moving method carries `deferred`). Focusing the
  pane that already has focus answers `false` at once.
- `view/close` (**differs**, additive): closing the **focused** pane moves
  the user's focus, so it honours `interrupt` like `view/focus`; closing any
  other pane never waits. Unknown `viewId`: `not_found`; the last pane:
  `unsupported`.
- `editor/select`: `primary` defaults to 0 and must index into `ranges`
  (`invalid_params`, `field: "primary"`); an empty `ranges` is
  `invalid_params`. Helix sorts the ranges and merges overlapping ones; the
  primary follows its range. Positions are clamped as elsewhere. With
  `"center": false` the view only scrolls as far as needed. Result
  `{ "version", "deferred" }`; when deferred, `version` is the current one.
  A `viewId` other than the focused pane does not move focus, but the
  request still honours `interrupt` (it scrolls a pane the user can see).

### Not yanking the user

- The user is busy when any of these holds (the last two are **additions**
  to PROTOCOL.md):
  - Helix is in insert mode;
  - a key was pressed in the last 1500 ms: any key bound to a command and any
    character typed (hooks on `PostCommand` and `PostInsertChar`); keys the
    bridge itself feeds do not count, mouse events do not count;
  - a key sequence is half typed (`<space>`, `g` …);
  - a prompt, picker or dialog is open (keys typed there fire no event, and a
    jump behind an open `:` line would make the user's command run in the
    wrong buffer). Popups (hover, signature help, completion) do not count.
- Consequence for agents: after `editor/command` leaves Helix in insert mode
  or with a prompt or picker open, the next `whenIdle` request waits. Finish
  in one request, or pass `"interrupt": "always"`.
- A deferred request is re-validated when `:bb-go` performs it; an error then
  (the file was deleted, the pane closed, the policy refuses it) goes to the
  status line. The slot does not expire; a newer request replaces it.
- The notice is `bb wants to <what> (:bb-go)`: `show <path>:<line>` (path
  relative to Helix's working directory when inside it), `close the pane
  showing <path>`, `run :<line>`, `run <command>`, `press <keys>`. Like any
  status message it is cleared by the user's next key, so it is shown again
  once they pause for 1500 ms with the status line empty (for up to 60 s).
- `:bb-go` with nothing pending fails with "Nothing from bb is waiting".
- `editor/open` deferred: `{ "docId", "path", "version", "viewId": null,
  "deferred": true }`, where `docId` and `version` are those of the buffer if
  it is already open and `null` otherwise; nothing is loaded. `not_found`
  and an unknown `viewId` are reported before deferring. `"focus": false`
  never waits.
- For the locations code (`locations/set` with `goto`, and later
  `editor/reload`/`editor/reveal` with `follow`): call
  `methods::interrupt::jump(editor, compositor, interrupt, what, act)`,
  which runs `act` now or keeps it as the pending jump and returns
  `Ok(None)`; parse the parameter with `Interrupt::parse`.

### `editor/command`

- Result: `{ "deferred": false, "mode", "status"?, "overlay"?,
  "pendingKeys"? }`. `status` is present only when the command put a message
  on the status line (Helix's `hint` is reported as `info`); when it put none
  the user's previous message is left in place. Messages that arrive later
  from async work (a language server's answer, a save) are not included.
  `overlay` names what the command left on top of the editor: `"prompt"`,
  `"picker"`, `"popup"` or `"other"`; `pendingKeys` is a half-typed key
  sequence, for example `"<space>"`. Nothing ever waits for more input: the
  editor is simply left in that state.
- Deferred: `{ "deferred": true }`. The policy is checked again when it runs.
- `typed`: a leading `:` is accepted. An unknown command (or an empty line)
  is `invalid_params` with `field: "typed"`; a bare number is `goto`, as on
  the command line. An error from the command itself is not an RPC error: it
  is shown to the user, as Helix shows it, and returned as `status` with
  severity `error`.
- `static`: the exact names of Helix's static command list (not the `:typed`
  or `@macro` forms a key map also accepts). `count` must be at least 1. A
  command that waits for the next key (`find_next_char`, `replace`,
  `select_register` …) is `unsupported`; send it with `keys`. The command
  runs as a bound key would: `PostCommand` and mode-switch hooks, then the
  compositor callbacks.
- `keys`: all keys are delivered in one turn of the main loop. Each request
  starts by dismissing a key sequence or a "next key" command the user (or an
  earlier request) left half done, so a sequence cannot be continued across
  requests. A picker that waits for a language server appears after the
  response; send the keys for it in a second request.
- Refusals are `unsupported` (1007) with `data.reason` (and `data.command`
  when a command is to blame); for `keys`, also `data.key` and `data.sent`
  (keys delivered before the refused one). After a refused key, Escape is
  sent.
- Policy (**differs**, stricter than the table in PROTOCOL.md): the full list
  and the reason for each refusal is `bb-bridge/COMMAND_POLICY.md`. Beyond
  PROTOCOL.md's list `safe` also refuses: the `write!` family (overwrites a
  file changed on disk), `config-reload`, `workspace-trust`,
  `workspace-untrust`, `workspace-exclude` (they load configuration, which
  can name programs), `:set`/`:toggle` of `shell`, `clipboard-provider`,
  `terminal`, `workspace-trust` and `bridge`, `lsp-workspace-command`, the
  debugger, the `:bb-*` commands that speak to bb in the user's name,
  `command_palette`, `suspend`, and keys bound to macros. `reload`/`rl` is
  refused while the buffer is modified, `reload-all` while any buffer is.
- The policy in force is the stricter of the one at start and the current
  configuration; `bridge/hello` reports that one. `:set bridge.commands
  none` (under `all`) takes effect at once; nothing short of a restart
  loosens it.

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
- **Last line of the document** (a file without a final newline, or a line
  past the end clamped to it): Helix lays out no virtual lines after it, so
  the text is drawn in the empty area below the end of the file. While the
  cursor is on that line (after each command, and after `annotations/set`),
  the view scrolls just enough for those rows to fit, never moving the
  cursor line out of view; the rows of any other line come into view as the
  cursor moves down past them. With the cursor elsewhere it can still be
  below the bottom of the view, like any line's rows.
- `annotations/clear` answers `{}`. Without `path` it clears the namespace in
  every open document and every file waiting to be opened.
- `:bb-annotations` hides and shows every bridge annotation in the process;
  it is not saved. Removed lines of a running reveal still show.

## Reveal (step 2d)

- Result: `{ "animated": <ranges with something on screen>, "deferred" }`.
  `follow` moves the focused view (cursor to the start of the first range,
  centred, opening the file in the focused pane if needed) through the "not
  yanking the user" gate: while the user is busy the move becomes the
  pending jump (`bb wants to show <path>:<line> (:bb-go)`), the result is
  `"deferred": true`, and what is on screen now animates without moving
  (`animated` counts that). `:bb-go` moves there and plays the reveal again,
  against the text as it is then. Without `follow`, `deferred` is `false`.
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
  at once. Whitespace is never covered. A visual line whose text is all
  still covered is drawn blank, so its indent guides, soft-wrap indicator
  and any inlay hints do not show before its text; the cursor, selection
  and cursorline backgrounds stay.
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
## Step 2a: reload, locations, applyEdits

### editor/reload and the focus check

- "Changed on disk" means the file's mtime differs from the one Helix
  recorded when it last read or wrote the buffer (different, not only
  newer). If the mtime moved but the content equals the buffer, the buffer
  is `unchanged` (and the new mtime is recorded when the buffer is clean).
- `paths` that are not open buffers are ignored: they appear in none of the
  four lists. A file that exists but cannot be read is reported in `missing`
  (and logged).
- A reload is applied as Helix's own `:reload` applies it: one transaction
  computed by Helix's diff (line by line, then by character inside changed
  lines), one undo step (`u` brings back the text from before it), the
  buffer clean afterwards. Selections and each view's scroll anchor are
  mapped through the diff in every view showing the buffer; the cursor is
  then kept in view. The language server gets a normal `didChange` and a
  `didChangeWatchedFiles` (`file_changed`), as with `:reload`. The diff
  base and branch name are refreshed.
- After a reload, `event/documentChanged` with `"modified": false` follows
  at once (not only at the next key press, as the step 1 note says for
  `:reload`).
- With `force`, modified buffers are reloaded and their unsaved changes
  replaced; the replaced text is still one `u` away.
- `follow` moves to the first change of the first reloaded buffer, in the
  order Helix lists its buffers, through the same jump as `locations/set`
  `goto` (jump list entry, centred), so it honours `interrupt`. Only with
  `follow` does the result carry `"deferred"`.
- `reveal` plays the reveal animation (`editor/reveal`, 600 ms) once per
  reloaded buffer with the text the reload inserted and the lines it
  removed, after the `follow` jump, so the followed buffer animates where
  the view landed. When the follow is deferred, what is on screen animates
  now and the followed buffer's reveal plays again on `:bb-go`, if the
  buffer has not changed since the reload (its ranges would not match).
- **`event/externalChange`** is sent once per change on disk, per buffer:
  the same buffer is announced again only after its file's mtime moves
  again. Both `editor/reload` (for `skippedModified` buffers) and the focus
  check send it.
- **Focus check**: when the terminal reports that it regained focus (Helix
  turns focus reporting on; a terminal that never reports focus never
  triggers it), clean buffers changed on disk are reloaded silently, exactly
  as `editor/reload` without `force` would, and buffers with unsaved changes
  are announced with `event/externalChange` and a warning on the status line
  naming them. It runs whether or not a client is connected, but only while
  the bridge runs (`enable = false` and `HELIX_BRIDGE=0` turn it off too).

### locations

- `title` is optional (default: the list name) and is shown when the picker
  opens. More than 2000 items is `invalid_params` with `field: "items"`; a
  `goto` past the end is `invalid_params` with `field: "goto"`.
- Item paths are resolved when the list is set (relative to
  `workspaceRoot`). An item's `col` places the cursor (default column 1);
  `endLine` selects from there through that line, its line ending
  included, as `editor/open` with `endLine` and no `endCol` does.
- The result is `{ "count", "deferred" }`; `deferred` is always present.
  `goto` honours `interrupt` like `editor/open`; a deferred goto is
  performed by `:bb-go` (the list is set and activated at once either way).
  A `goto` to an item whose file is gone does not move, shows a warning and
  still succeeds; deferred, the warning comes when `:bb-go` runs.
- `locations/set` replaces the list of the same name; `"activate": false`
  leaves the active list as it was. `locations/clear` without `list` clears
  every list; it returns `{}`. Lists last for the session.
- `:bb-locations [list]` opens the named list, else the active one, else the
  most recently created. Columns: file, line (`5-6` for a range), label,
  note, with a preview of the lines. Enter, `C-s` and `C-v` open as in any
  picker. The list becomes active.
- `:bb-next` and `:bb-prev` continue from the item last visited or stood
  on; with none yet, `:bb-next` goes to the first item and `:bb-prev` to the
  last. Items whose file is neither open nor on disk are skipped. The status
  line reads `<list> <i>/<n>: <note>` (the label if there is no note, then
  `path:line`), with `(skipped N missing file(s), wrapped)` when that
  happened. No active list, an empty list, or every file gone is a
  status-line error.
- **The note under the cursor**: after every command run from a key, if the
  primary cursor's line lies within an item of the active list (`line` to
  `endLine`) that has a note, the status line shows that item as above. It
  never replaces a message another command just set, and it makes that item
  the current one for `:bb-next`. Mouse clicks do not trigger it. With no
  active list that has notes, the hook is one relaxed atomic load.
- Every jump (goto, walk, picker) records where the user was in the jump
  list, so `C-o` goes back.

### editor/applyEdits

- Positions are clamped as everywhere else: a column past the end of a line
  is the end of that line before its line ending (an edit never splits a
  CRLF), a line past the end is the end of the text. A range whose start is
  after its end is `invalid_params` (`field: "edits"`).
- Overlap: touching ranges are fine. Insertions at the same point are
  applied in the order given, and before a replacement that starts there.
  An insertion strictly inside a replaced range overlaps it.
- `newText` is inserted exactly as given; `\n` is not converted to the
  buffer's line ending.
- `version` is compared with the buffer as it is when the request reaches
  the editor. A file that was not open has version 0 once opened, so a
  `version` sent for a file that was closed in the meantime normally fails
  with `version_mismatch`.
- Undo: unsaved typing in progress (insert mode) is committed as its own
  undo step first, then the edits are one step, so one `u` removes exactly
  the edits and the user's typing is never merged with them.
- `label` is shown on the status line.
- A file that is not open is opened in the background and stays open as a
  hidden buffer (listed in `editor/state` `documents`).
- `save` goes through Helix's own save queue (language server `didSave`,
  `event/documentSaved`, the "written" message) and the response waits for
  the write, up to 2 s; `modified` is then relative to the revision written.
  If the file changed on disk since Helix read it, where Helix's own `:w`
  would refuse, the request fails before anything is applied with
  `unsupported` and `data: { "reason": "changed on disk" }`. If the write
  itself fails after the edits were applied: `unsupported` with
  `data: { "reason": "save failed", "applied": true, "version" }`; if it does
  not finish in time, `timeout` with `data.applied: true`.

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
