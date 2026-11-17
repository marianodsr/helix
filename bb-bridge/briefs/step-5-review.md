# Brief: reviewing changes inside the editor (fork side)

The owner wants to review changes in the same Helix they edit in: see a set
of changes as a diff, move through it, comment on lines and send the comments
to the agent. Requirement 8 in `bb-bridge/ARCHITECTURE.md`; the contract is
the new "review/set, review/clear, review/state" section of
`bb-bridge/PROTOCOL.md`, the `event/review` row, and the `:bb-review` and
`:bb-diff` commands. Read ARCHITECTURE.md (the patch rules and the
performance targets are binding), PROTOCOL.md, PROTOCOL_NOTES.md,
UPSTREAM_TOUCHPOINTS.md and `bb-bridge/briefs/step-2-common.md` (how work is
done here: your own files, additive edits to shared ones, tests, live checks,
the owner is using the editor), then the bridge code, especially
`methods/annotations.rs`, `methods/reveal.rs` and `methods/locations.rs`,
which already draw virtual lines, tint ranges and run pickers.

You work in your own git worktree branched from `bb-bridge`. Another agent is
building the bb plugin side against the same contract at the same time.

## Build (`methods/review.rs`, capability `review`)

1. **The base.** Helix compares each buffer with the file at `HEAD`
   (`helix-vcs`: `get_diff_base`, `Document::set_diff_base`, the diff
   handle and its hunks). While a review is active, compare with the file at
   the review's revision instead, for every open buffer and every buffer
   opened later, and go back to `HEAD` on `review/clear`. Read blobs off the
   main thread. Find the smallest touchpoint in `helix-vcs` for "the file at
   this revision"; renamed files compare against `oldPath`. Because it is
   Helix's own diff, the gutter and `]g` / `[g` follow for free and stay
   live while the user or an agent edits; check that they do.
2. **Inline diff.** Removed lines as virtual lines above the change, struck
   through, and a background on added and changed lines, in the theme's diff
   scopes (`diff.minus`, `diff.plus`, `diff.delta`), from the hunks Helix
   already computes. Decoration only. Reuse the annotation and reveal
   machinery; annotations and a reveal must still draw correctly on top.
   `:bb-diff` toggles it with or without a review; `[editor.bridge]
   inline-diff` sets the start state.
3. **The file list and picker.** `review/set` stores the list;
   `:bb-review` opens Helix's own picker over it with status, `+n -m` and
   path, and the file's diff as the preview; Enter opens the file at its
   first change. A deleted file opens its base text in a read-only scratch
   buffer. Re-sending `review/set` with the same id replaces the list in
   place.
4. **`:bb-review <target>`** emits `event/review`; `uncommitted` and `off`
   work with no client. Completion for the target words.
5. **`event/send`** carries `reviewId` while a review is active.
6. `review/state`, `review/clear`, and `open` with the interrupt gate.

## Things to get right

- **Cost.** With no review and the inline diff off: nothing per key, nothing
  per frame. With it on, in a 20k-line file with hundreds of hunks, moving
  and typing stay as fast as with the same number of diagnostics; measure
  with the key-press benchmark and `bench/move_latency.py`, and report. The
  hunks are recomputed by Helix on every edit; your decorations must follow
  without rebuilding everything on the main thread.
- **Looks.** The owner reads diffs here all day. Capture (with
  `bench/look.py`) in their theme and a light one: a modified block, a pure
  addition, a pure deletion, a change next to a comment annotation, a long
  removed block, soft-wrapped lines, a narrow split, and the picker with its
  preview. Look at them and fix what reads badly. Removed text must be
  clearly not part of the file; added lines must stay readable with syntax
  colours.
- **Editing during review.** The user edits the file while reviewing: the
  inline diff updates as they type, undo works, and nothing they type lands
  in a virtual line.
- **Edge cases.** A file new in the review (all added), a file outside the
  repository, a repository with no commits, a base revision that is missing,
  two repositories open at once, CRLF files, and a 50k-line file.
- **Command policy.** Decide the verdict for `:bb-review` and `:bb-diff`
  under `safe` and regenerate `COMMAND_POLICY.md`.

## Done when

Build, unit and integration tests, upstream's suite, fmt, clippy on the
bridge and `live.py verify` under a pty with a release build all pass;
`review` is in `bridge/hello`; the captures are in your report.

Commit on your branch. Do not push, merge or spawn bb threads. Report as
`step-2-common.md` says, with the captures and the numbers.
