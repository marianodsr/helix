# Bridge benchmarks and live checks

Everything here runs against a build of this branch; nothing touches the
user's own Helix config, cache or terminals (throwaway `HOME` and XDG dirs).

| Tool | What it does |
|---|---|
| `bridge_client.py` | Minimal client library (standard library only). |
| `rtt.py` | Round trip of one method against a running instance: `rtt.py --method editor/state -n 2000`. |
| `live.py verify` | Starts `target/release/hx` under a pty and checks every step 1 method, event and command over the socket: two clients, a client that stops reading (overflow), one that never reads (dropped), clean exit, long cache path, off switches. Prints PASS/FAIL. |
| `live.py bench` | Round trips, event latency, memory per idle client, startup cost, on a real `hx`. |
| `live.py compat --config DIR` | Starts `hx` with `DIR/helix` as its config, read-only, and reports anything config, languages or any theme in `DIR/helix/themes` complain about. |
| `live_lsp.py` | Step 2b, used by `live.py`: `verify` also builds a small Go module (packages, an interface with two implementations, non-ASCII names) and checks `diagnostics/get`, `lsp/servers`, every `lsp/query` kind, errors, cancel and both events against `~/go/bin/gopls`. `live.py lspbench` measures `lsp/query` overhead above gopls's own time and `diagnostics/get` on ~3000 diagnostics; `live.py lspbig --repo DIR --file F --query '{"position":{"line":L,"col":C}}'` does the same on a large repository. |
| `live.py redraw --before BIN` | Integration step: a 20k-line Go file with gopls, `editor/state` round trip, `lsp/query` `hover` overhead above gopls and `editor/status` (which must still repaint, checked on screen), for `BIN` against `--hx`. Build `BIN` with the `skip_render()` condition in `application.rs` disabled. |
| `keypress.rs`, `keypress.sh` | Key-press benchmark in Helix's integration harness, this branch (bridge off / listening / one client subscribed to everything) against the base commit. |

```sh
cargo build --release
python3 bb-bridge/bench/live.py verify
python3 bb-bridge/bench/live.py bench
python3 bb-bridge/bench/live.py compat --config bb-bridge/config
bb-bridge/bench/keypress.sh            # release builds into target/bench*
```

`live.py` needs a pty, which some sandboxes refuse ("out of pty devices").
