# Brief: step 2d, annotations and the reveal animation

You own `methods/annotations.rs` and a new file `methods/reveal.rs`. Read
`bb-bridge/briefs/step-2-common.md` first; its rules are binding. Three other
agents are building 2a, 2b and 2c in their own worktrees at the same time.

The owner asked for two things, in their words: visual hints inside Helix
when they leave a comment, and seeing the agent write in real time.
Requirements 6 and 7 in ARCHITECTURE.md.

## Build

- **`annotations/set`, `annotations/clear`, `:bb-annotations`** exactly as
  PROTOCOL.md: a gutter sign on the marked lines, an optional subtle line
  background, and the text as virtual lines under the range, wrapped and
  indented to the code, stacking when several share a line. Capability
  `annotations`.
  - Helix already draws virtual text: inlay hints, inline diagnostics and
    line annotations in the text-annotation machinery, and gutters are
    pluggable. Study how inline diagnostics render their virtual lines and
    how the diagnostics and diff gutters draw, and build on the same
    mechanisms rather than a new render path.
  - Style from theme scopes that every theme defines (for example the
    diagnostic, comment and `ui.virtual` scopes), so it reads well in the
    owner's `catppuccin_macchiato` and in any other theme. Comments,
    replies and notes must be distinguishable from each other and from
    Helix's own diagnostics at a glance.
  - Annotations follow their lines through edits (map positions through
    changes the way diagnostics do), and disappear with their lines.
  - They are never part of the document: not selectable, not yanked, not
    saved, not sent to the language server, no effect on line numbers.
- **`editor/reveal`** exactly as PROTOCOL.md, in `methods/reveal.rs`, with
  capability `reveal` and the `[editor.bridge] reveal` switch.
  - The document always holds the final text. The animation is decoration
    only: cover the not-yet-revealed part of each range (overlay
    annotations can replace graphemes on screen) and highlight what was just
    uncovered, advancing on a timer that requests redraws only while an
    animation is running, and stopping the timer when it ends.
  - Any key press ends every animation at once.
  - `removed` lines show struck through (or dimmed, if the theme has no
    strike-through) above their position for the same time.
  - Expose one function, `reveal::play(editor, doc, ranges, removed,
    duration, follow)`, that the reload code will call after the merge with
    the ranges its own diff produced. The method is a thin wrapper over it.
  - `follow` and `interrupt` move the view; step 2c owns the waiting
    behaviour, so for now move at once and return `"deferred": false` from
    one call site.

## Things to get right

- **Cost.** With no annotations and no animation there is no per-frame or
  per-key work. With hundreds of annotations in a 20k-line file, scrolling
  and typing stay as fast as with the same number of diagnostics; measure
  with the key-press benchmark and report. An animation never delays a key.
- **Looks.** This is the part of the bridge the owner sees all day. Capture
  what it looks like (the pty output rendered to text or an image is fine)
  for: one comment, a comment with a reply, three stacked, a long comment
  that wraps, a narrow split, the owner's theme and a light theme, and a
  reveal mid-animation. Look at them and fix what reads badly.
- **Edge cases.** Annotations on the last line, on a line that soft-wraps
  (the owner has soft wrap on), in two views of the same buffer with
  different widths, and across undo and reload.

Report as the common file says, with the captures.
