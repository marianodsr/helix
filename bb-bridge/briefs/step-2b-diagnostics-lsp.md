# Brief: step 2b, diagnostics and language-server queries

You own `methods/diagnostics.rs` and `methods/lsp.rs`. Read
`bb-bridge/briefs/step-2-common.md` first; its rules are binding.

The point of this piece: an agent uses the language server Helix already
runs (for the owner, gopls on a very large Go monorepo) instead of starting a
second one. It must be correct about positions and must add almost nothing on
top of the server's own time.

## Build

- **`diagnostics/get`** exactly as PROTOCOL.md, including `counts`,
  `minSeverity`, paging and `limit: 0`. Sorting and serialising happen off
  the main thread. Capability `diagnostics`.
- **`event/diagnostics`** (coalesced per path) and **`event/lsp`** (server
  initialized or exited), from the existing Helix events.
- **`lsp/servers`**: running servers with name, languages, root,
  initialized, and the features each supports.
- **`lsp/query`** for every kind in the table: `definition`,
  `typeDefinition`, `implementation`, `references`, `hover`,
  `documentSymbols`, `workspaceSymbols`, `incomingCalls`, `outgoingCalls`,
  `renamePreview`. Use Helix's own client methods. Results are normalised to
  the protocol's `Location` and `Symbol`, with `preview` lines read from the
  open buffer when there is one and from disk otherwise, off the main thread.
  `limit`, `truncated`, `timeoutMs` and `$/cancel` work; a cancelled or timed
  out query cancels the request to the server. Capability `lsp`.
- **Choosing the server**: from the file's language and the feature needed,
  the way Helix picks one for its own commands. With `ensure` (default true)
  a file that is not open is loaded in the background so the server knows it,
  without changing what the user sees. No server: `no_language_server`, with
  the language in `data`.

## Things to get right

- **Positions.** The protocol uses 1-based lines and columns in Unicode
  scalar values; servers use 0-based positions in their negotiated encoding
  (UTF-8, UTF-16 or UTF-32). Convert through Helix's own helpers with the
  server's offset encoding, in both directions, for open buffers and for
  files only on disk. Test lines with multi-byte characters before and after
  the position.
- **The main thread** only issues the request and reads small state. Awaiting
  the server, building previews and JSON all happen off it.
- **Unsaved buffers**: the server sees Helix's buffer contents, so results
  reflect unsaved text; previews for open buffers come from the buffer.
- **Verify with a real gopls** (`~/go/bin/gopls` is installed; `go` 1.25 is on
  PATH) on a small Go module you create in a temp folder with several
  packages, an interface with two implementations, and non-ASCII
  identifiers or strings. Every kind, plus the error paths.
- **Measure** the overhead of `lsp/query` above the server's own time
  (target: under 3 ms) and `diagnostics/get` on a few thousand diagnostics.
- If you have time left, repeat the measurements on a large public Go
  repository (a shallow clone in a temp folder is fine) and report memory and
  timings; do not leave gopls running afterwards.

Report as the common file says.
