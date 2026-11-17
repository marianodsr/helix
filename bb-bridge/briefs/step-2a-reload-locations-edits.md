# Brief: step 2a, reload, locations and applyEdits

You own `methods/reload.rs`, `methods/locations.rs` and `methods/edits.rs`.
Read `bb-bridge/briefs/step-2-common.md` first; its rules are binding.

## Build

- **`editor/reload`** exactly as PROTOCOL.md: reload open buffers whose file
  changed on disk, skip buffers with unsaved changes unless `force`, report
  `reloaded`, `unchanged`, `skippedModified`, `missing`. Selections and
  scroll position survive as far as the new text allows, in every view that
  shows the buffer. The language server is told about the new text the way
  Helix's own `:reload` tells it. Capability `reload`.
- **Focus check.** When the terminal regains focus, compare the mtime of each
  open buffer's file with what Helix last read or wrote: reload the clean
  ones silently, and for buffers with unsaved changes emit
  `event/externalChange` once per change and show a status message. No file
  watcher, and no work when nothing changed. Find the smallest hook for the
  terminal's focus-gained event.
- **`event/externalChange`** also when `editor/reload` skips a modified
  buffer.
- **`locations/set`, `locations/clear`**, named lists, `activate`, `goto`,
  the 2000-item limit; **`:bb-locations [list]`** as a picker in Helix's own
  picker UI with file, line, label and a preview; **`:bb-next`, `:bb-prev`**
  walking the active list with wrap-around and a status line like
  `tour 2/5: <note>`. While the cursor stands on an item its `note` is on the
  status line. Items whose file is gone are skipped with a message, not an
  error. Capability `locations`. `goto` moves at once for now and returns
  `"deferred": false`; step 2c adds the waiting behaviour, so make the jump
  one function with one call site.
- **`editor/applyEdits`** exactly as PROTOCOL.md: one undoable transaction,
  ranges against the text before any edit, overlapping ranges refused with
  `invalid_params`, `version` check with `version_mismatch`, background open
  when the file is not open, `save`. Edits are visible at once in every view
  and the language server is notified like any other change. Capability
  `applyEdits`.

## Things to get right

- Position conversion is in `convert.rs`; multi-byte text, CRLF and end of
  file are where edits and jumps go wrong. Test them.
- An edit or reload must never lose undo history the user would expect to
  keep, and `u` after `applyEdits` restores the buffer exactly.
- A reload of a 20k-line file must not stall the editor noticeably; measure
  it and say what you measured.

Report as the common file says.
