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
| `keypress.rs`, `keypress.sh` | Key-press benchmark in Helix's integration harness, this branch (bridge off / listening / one client subscribed to everything) against the base commit. |

```sh
cargo build --release
python3 bb-bridge/bench/live.py verify
python3 bb-bridge/bench/live.py bench
python3 bb-bridge/bench/live.py compat --config bb-bridge/config
bb-bridge/bench/keypress.sh            # release builds into target/bench*
```

`live.py` needs a pty, which some sandboxes refuse ("out of pty devices").
