"""Integration step: no redraw after read-only requests. Used by `live.py redraw`.

Starts each binary on a 20k-line Go file (gopls running, the view in the
middle of the file) and measures, one request at a time:

- `editor/state` round trip (read-only: no frame after the change);
- `lsp/query` `hover` overhead above gopls's own time (read-only);
- `editor/status` round trip (changes the screen: still one frame each),
  and that its message really reaches the terminal.

    live.py redraw --before target/bench-redraw/hx-before [--hx target/release/hx] [-n 1000]
"""

import os
import time

import live_lsp
from bridge_client import Client
from rtt import measure


def big_go():
    lines = ["package main", ""]
    for i in range(4000):
        lines += [f"func g{i}(x int) int {{", f"\treturn x * {i}", "}", "", f"// g{i} ends"]
    return "\n".join(lines[:20000]) + "\n"


def run_one(hx_path, Sandbox, n):
    sb = Sandbox()
    live_lsp.make_module(sb, {"go.mod": "module example.com/big\n\ngo 1.25\n"})
    sb.write("big.go", big_go())
    big = sb.path("big.go")
    hx = live_lsp.start(sb, hx_path, files=("big.go:10000",))
    out = {}
    try:
        reg = sb.registry(hx.pid, timeout=30)
        if reg is None:
            with open(sb.log) as f:
                raise SystemExit(f"{hx_path} did not start:\n{f.read()[-2000:]}")
        c = Client(reg["socket"], timeout=60)
        c.call("bridge/hello", {"client": "redraw"})
        if not live_lsp.wait_server(c):
            raise SystemExit("gopls did not start")
        # line 10002 is `\treturn x * 2000`; hover on `x`
        hover = {"kind": "hover", "path": big, "position": {"line": 10002, "col": 9}}
        for _ in range(100):  # until gopls has loaded the package
            try:
                if c.call("lsp/query", hover)["markdown"]:
                    break
            except Exception:
                pass
            time.sleep(0.2)
        time.sleep(1.0)
        out["editor/state"] = measure(c, "editor/state", {}, n)
        o = live_lsp.overhead(c, hover, max(n // 5, 50))
        out["lsp/query hover overhead"] = o["overhead"]
        out["lsp/query hover rtt"] = o["rtt"]
        out["lsp/query hover gopls"] = o["server"]
        out["editor/status"] = measure(c, "editor/status", {"message": "bench"}, max(n // 5, 50))
        m = hx.mark()
        c.call("editor/status", {"message": "REDRAW-PROBE-7"})
        out["status shown"] = hx.wait_text("REDRAW-PROBE-7", m, timeout=2.0)
    finally:
        live_lsp.stop(hx)
        sb.cleanup()
    return out


def row(name, r):
    if "p99" in r:
        return f"{name:<28} p50={r['p50']:.3f}ms p90={r['p90']:.3f}ms p99={r['p99']:.3f}ms max={r['max']:.3f}ms"
    return f"{name:<28} p50={r['p50']:.3f}ms p90={r['p90']:.3f}ms max={r['max']:.3f}ms"


def bench(args, Sandbox):
    n = args.n if args.n != 50 else 1000
    results = {}
    for label, path in (("before", args.before), ("after", args.hx)):
        print(f"--- {label}: {path}")
        r = run_one(os.path.abspath(path), Sandbox, n)
        results[label] = r
        for k, v in r.items():
            print(row(k, v) if isinstance(v, dict) else f"{k:<28} {v}")
    ok = results["after"]["status shown"] and results["before"]["status shown"]
    print("PASS editor/status still repaints" if ok else "FAIL editor/status message not on screen")
    return 0 if ok else 1
