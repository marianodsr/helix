"""Step 2b live checks and benchmarks: diagnostics/get, lsp/servers, lsp/query
and their events, against a real gopls. Used by `live.py verify`,
`live.py lspbench` and `live.py lspbig`.

Every run builds a small Go module in a throwaway sandbox (several packages,
an interface with two implementations, non-ASCII identifiers and strings)
and starts `hx` there with gopls and go on PATH. gopls is stopped with the
editor; anything left is killed.
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import time

from bridge_client import Client, RpcError

ESC = "\x1b"
ENTER = "\r"

GOPLS = os.path.expanduser("~/go/bin/gopls")

# Lines are referred to by number below; keep them in step.
FILES = {
    "go.mod": "module example.com/shapes\n\ngo 1.25\n",
    # line 3: type Shape (col 6), line 4: Area (col 2)
    "shapes/shape.go": (
        "package shapes\n"
        "\n"
        "type Shape interface {\n"
        "\tArea() float64\n"
        "}\n"
    ),
    # line 3: type Círculo (col 6), line 5: method Area (col 18)
    "shapes/circle.go": (
        "package shapes\n"
        "\n"
        "type Círculo struct{ R float64 }\n"
        "\n"
        "func (c Círculo) Area() float64 { return 3.14 * c.R * c.R } // π≈3.14\n"
    ),
    "shapes/square.go": (
        "package shapes\n"
        "\n"
        "type Square struct{ S float64 }\n"
        "\n"
        "func (s Square) Area() float64 { return s.S * s.S }\n"
    ),
    # line 6: func Total (col 6); line 9: s.Area() (Area at col 10)
    "geo/total.go": (
        "package geo\n"
        "\n"
        'import "example.com/shapes/shapes"\n'
        "\n"
        "// Total adds up the areas. ¿Qué tal? 🦀\n"
        "func Total(xs []shapes.Shape) float64 {\n"
        "\tt := 0.0\n"
        "\tfor _, s := range xs {\n"
        "\t\tt += s.Area()\n"
        "\t}\n"
        "\treturn t\n"
        "}\n"
    ),
    # line 9: multi-byte chars before and after `Círculo` (col 39), `名前` (col 15)
    # line 10: geo.Total (Total at col 22), `c` (col 43)
    "main.go": (
        "package main\n"
        "\n"
        "import (\n"
        '\t"fmt"\n'
        '\t"example.com/shapes/geo"\n'
        '\t"example.com/shapes/shapes"\n'
        ")\n"
        "\n"
        'func main() { 名前 := "🦀é"; c := shapes.Círculo{R: 2}; _ = "é🦀"\n'
        "\tfmt.Println(名前, geo.Total([]shapes.Shape{c, shapes.Square{S: 3}}))\n"
        "}\n"
    ),
    # line 3: the error is after multi-byte chars: `"x"` at col 27
    "broken/broken.go": (
        "package broken\n"
        "\n"
        'var _ = "🦀é"; var y int = "x"\n'
        "\n"
        "func F() int { return undefinedThing }\n"
    ),
}


def go_env(sb):
    go = shutil.which("go") or "/opt/homebrew/bin/go"
    return {
        "PATH": ":".join([os.path.dirname(GOPLS), os.path.dirname(os.path.realpath(go)), os.path.dirname(go), "/usr/bin", "/bin"]),
        "GOTOOLCHAIN": "local",
        "GOPROXY": "off",
        "GOFLAGS": "-mod=mod",
        "GOPATH": os.path.join(sb.root, "gopath"),
        "GOCACHE": os.path.join(sb.root, "gocache"),
        "GOMODCACHE": os.path.join(sb.root, "gomodcache"),
        "GOENV": "off",
    }


def make_module(sb, files=FILES):
    for rel, text in files.items():
        os.makedirs(os.path.dirname(os.path.join(sb.repo, rel)), exist_ok=True)
        sb.write(rel, text)


def children(pid):
    out = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout
    return [int(p) for p in out.split()]


def stop(hx, timeout=8.0):
    """Quit hx cleanly; kill whatever it started that is still running."""
    kids = children(hx.pid)
    try:
        hx.esc()
        hx.send(":qa!" + ENTER)
        hx.wait_exit(timeout)
    finally:
        hx.kill()
        deadline = time.monotonic() + 3
        for pid in kids:
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
            else:
                try:
                    os.kill(pid, signal.SIGKILL)
                    print(f"note: killed leftover process {pid}")
                except ProcessLookupError:
                    pass


def wait_server(c, timeout=60.0, name="gopls"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        servers = c.call("lsp/servers")["servers"]
        for s in servers:
            if s["name"] == name and s["initialized"]:
                return s
        time.sleep(0.1)
    return None


def wait_diagnostics(c, path, timeout=60.0, at_least=1):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        r = c.call("diagnostics/get", {"path": path, "limit": 0})
        if r["total"] >= at_least:
            return r
        time.sleep(0.1)
    return None


def q(c, kind, path=None, line=None, col=None, **extra):
    params = {"kind": kind}
    if path:
        params["path"] = path
    if line:
        params["position"] = {"line": line, "col": col}
    params.update(extra)
    return c.call("lsp/query", params)


def err(c, method, params):
    try:
        c.call(method, params)
    except RpcError as e:
        return e.error
    return None


def start(sb, hx_path, files=("main.go",), args=("-v",)):
    return sb.start(hx_path, files=files, args=args, **go_env(sb))


# --- verify ------------------------------------------------------------------


def verify(args, Sandbox, check):
    if not os.path.exists(GOPLS):
        check(False, "lsp: gopls installed", GOPLS)
        return
    sb = Sandbox()
    make_module(sb)
    hx = start(sb, args.hx)
    try:
        _verify(sb, hx, check)
    finally:
        stop(hx)
        if not args.keep:
            sb.cleanup()


def _verify(sb, hx, check):
    P = sb.path
    main_go, total_go, circle_go = P("main.go"), P("geo/total.go"), P("shapes/circle.go")
    shape_go, square_go, broken_go = P("shapes/shape.go"), P("shapes/square.go"), P("broken/broken.go")
    reg = sb.registry(hx.pid)
    if not check(reg is not None, "lsp: registry file written"):
        return
    c = Client(reg["socket"], timeout=30)
    hello = c.call("bridge/hello", {"client": "live-lsp"})
    check("diagnostics" in hello["capabilities"] and "lsp" in hello["capabilities"], "lsp: capabilities", hello["capabilities"])
    c.call("events/subscribe", {"events": ["lsp", "diagnostics"]})

    server = wait_server(c)
    if not check(server is not None, "lsp/servers: gopls initialized", c.call("lsp/servers")):
        return
    check("go" in server["languages"] and os.path.realpath(server["root"] or "") == os.path.realpath(sb.repo),
          "lsp/servers: languages and root", server)
    check(all(k in server["features"] for k in ("definition", "references", "hover", "implementation", "renamePreview",
                                                  "incomingCalls", "documentSymbols", "workspaceSymbols")),
          "lsp/servers: features", server["features"])
    check(server.get("positionEncoding") in ("utf-8", "utf-16", "utf-32"), "lsp/servers: positionEncoding", server)

    # positions: multi-byte chars before and after, on a line from disk (open buffer)
    r = q(c, "definition", main_go, 9, 40)
    loc = r["locations"][0] if r["locations"] else {}
    check(loc.get("path") == circle_go and loc.get("range") == {"start": {"line": 3, "col": 6}, "end": {"line": 3, "col": 13}},
          "definition: Círculo after multi-byte chars", r)
    check(loc.get("preview") == "type Círculo struct{ R float64 }", "definition: preview from disk", loc)
    r = q(c, "definition", main_go, 10, 14)
    check(r["locations"] and r["locations"][0]["path"] == main_go and r["locations"][0]["range"]["start"] == {"line": 9, "col": 15},
          "definition: 名前 (non-ASCII identifier) in the open buffer", r)
    r = q(c, "typeDefinition", main_go, 10, 43)
    check([l["path"] for l in r["locations"]] == [circle_go], "typeDefinition: c is a Círculo", r)
    r = q(c, "implementation", shape_go, 3, 7)
    check(sorted(l["path"] for l in r["locations"]) == sorted([circle_go, square_go]) and r["total"] == 2,
          "implementation: two implementations of Shape", r)
    r = q(c, "implementation", circle_go, 5, 19)
    check([l["path"] for l in r["locations"]] == [shape_go], "implementation: Círculo.Area implements Shape.Area", r)
    r = q(c, "references", total_go, 6, 7)
    paths = sorted(l["path"] for l in r["locations"])
    check(paths == sorted([total_go, main_go]), "references: Total (declaration included)", r)
    r2 = q(c, "references", total_go, 6, 7, includeDeclaration=False)
    check(r2["total"] == r["total"] - 1, "references: includeDeclaration false", r2)
    ref_main = [l for l in r["locations"] if l["path"] == main_go]
    check(ref_main and ref_main[0]["range"]["start"] == {"line": 10, "col": 22}
          and ref_main[0]["preview"].startswith("\tfmt.Println(名前, geo.Total("), "references: position and preview in main.go", ref_main)
    r = q(c, "references", total_go, 6, 7, limit=1)
    check(len(r["locations"]) == 1 and r["total"] == 2 and r["truncated"], "references: limit and truncated", r)
    r = q(c, "references", total_go, 6, 7, limit=1, offset=1)
    check(len(r["locations"]) == 1 and not r["truncated"], "references: offset", r)
    r = q(c, "hover", total_go, 6, 7)
    check("func Total" in r["markdown"], "hover: markdown", r)
    r = q(c, "hover", total_go, 2, 1)
    check(r == {"markdown": ""}, "hover: nothing there is an empty string", r)
    r = q(c, "documentSymbols", circle_go)
    names = [s["name"] for s in r["symbols"]]
    check("Círculo" in names and any("Area" in n for n in names), "documentSymbols: Círculo and its method", r)
    sym = next((s for s in r["symbols"] if s["name"] == "Círculo"), {})
    check(sym.get("kind") == "struct" and sym.get("location", {}).get("range", {}).get("start") == {"line": 3, "col": 6},
          "documentSymbols: kind and position", sym)
    r = q(c, "workspaceSymbols", query="Círculo")
    check(any(s["name"] == "Círculo" and s["location"]["path"] == circle_go for s in r["symbols"]),
          "workspaceSymbols: no path, from the focused document's server", r)
    r = q(c, "workspaceSymbols", main_go, query="Square")
    check(any(s["name"] == "Square" and s["location"]["path"] == square_go for s in r["symbols"]), "workspaceSymbols: with a path", r)
    # previews: off by default for the symbol kinds, on for the location kinds
    check("preview" not in sym.get("location", {}), "documentSymbols: no preview by default", sym)
    r = q(c, "documentSymbols", circle_go, previews=True)
    sym = next((s for s in r["symbols"] if s["name"] == "Círculo"), {})
    check(sym.get("location", {}).get("preview") == "type Círculo struct{ R float64 }", "documentSymbols: previews on request", sym)
    r = q(c, "workspaceSymbols", main_go, query="Círculo")
    sym = next((s for s in r["symbols"] if s["name"] == "Círculo"), {})
    check(sym.get("location", {}).get("range", {}).get("start") == {"line": 3, "col": 6} and "preview" not in sym["location"],
          "workspaceSymbols: position from disk, no preview", sym)
    r = q(c, "definition", main_go, 9, 40, previews=False)
    check(r["locations"] and "preview" not in r["locations"][0] and r["locations"][0]["range"]["start"] == {"line": 3, "col": 6},
          "definition: previews false", r)
    r = q(c, "incomingCalls", total_go, 6, 7)
    call = r["calls"][0] if r["calls"] else {}
    check(call.get("name") == "main" and call.get("location", {}).get("path") == main_go
          and call.get("callSites", [{}])[0].get("range", {}).get("start") == {"line": 10, "col": 22},
          "incomingCalls: main calls Total, with the call site", r)
    r = q(c, "outgoingCalls", main_go, 9, 7)
    names = sorted(x["name"] for x in r["calls"])
    check("Total" in names and "Println" in names, "outgoingCalls: main calls Println and Total", r)
    before = open(total_go, encoding="utf-8").read()
    r = q(c, "renamePreview", total_go, 6, 7, newName="Suma")
    check(sorted(r["files"]) == sorted([total_go, main_go]) and all(e["newText"] == "Suma" for e in r["edits"]),
          "renamePreview: edits in both files", r)
    e_main = [e for e in r["edits"] if e["path"] == main_go]
    check(e_main and e_main[0]["range"] == {"start": {"line": 10, "col": 22}, "end": {"line": 10, "col": 27}},
          "renamePreview: range after multi-byte chars", e_main)
    check(open(total_go, encoding="utf-8").read() == before, "renamePreview: nothing applied")
    e = err(c, "lsp/query", {"kind": "renamePreview", "path": total_go, "position": {"line": 6, "col": 7}, "newName": "no good"})
    check(e and e["code"] == 1007 and e.get("data", {}).get("server") == "gopls", "renamePreview: server refuses a bad name", e)

    # files that are not open: ensure (default) loads and closes again; ensure false reads disk
    st0 = c.call("editor/state")
    r = q(c, "definition", total_go, 9, 11)
    check([l["path"] for l in r["locations"]] == [shape_go], "ensure: definition from a file that is not open", r)
    time.sleep(0.3)
    st1 = c.call("editor/state")
    check([d["path"] for d in st1["documents"]] == [d["path"] for d in st0["documents"]] and st1["focused"]["path"] == main_go,
          "ensure: the background buffer is closed again, focus unchanged", st1["documents"])
    r = q(c, "definition", total_go, 9, 11, ensure=False)
    check([l["path"] for l in r["locations"]] == [shape_go], "ensure false: position from the file on disk", r)
    st2 = c.call("editor/state")
    check(len(st2["documents"]) == len(st0["documents"]), "ensure false: nothing opened")

    # unsaved text: insert a line at the top of main.go without saving
    hx.esc()
    hx.send("gg")
    hx.send("O// añadido 🦀")
    hx.esc()
    time.sleep(0.5)
    r = q(c, "definition", main_go, 10, 42)
    check(r["locations"] and r["locations"][0]["path"] == circle_go, "unsaved buffer: positions follow the unsaved text", r)
    r = q(c, "references", total_go, 6, 7)
    ref_main = [l for l in r["locations"] if l["path"] == main_go]
    check(ref_main and ref_main[0]["range"]["start"]["line"] == 11 and ref_main[0]["preview"].startswith("\tfmt.Println("),
          "unsaved buffer: references and previews from the buffer", ref_main)
    hx.send("u")
    time.sleep(0.3)

    # errors
    e = err(c, "lsp/query", {"kind": "definition", "path": P("notes.md"), "position": {"line": 1, "col": 1}})
    check(e and e["code"] == 1004 and e.get("data", {}).get("language") == "markdown", "no_language_server with the language", e)
    e = err(c, "lsp/query", {"kind": "definition", "path": P("nope.go"), "position": {"line": 1, "col": 1}})
    check(e and e["code"] == 1001, "not_found: missing file", e)
    e = err(c, "lsp/query", {"kind": "definition", "path": P("shapes"), "position": {"line": 1, "col": 1}})
    check(e and e["code"] == 1001, "not_found: a directory", e)
    e = err(c, "lsp/query", {"kind": "definition", "path": main_go})
    check(e and e["code"] == -32602 and e["data"]["field"] == "position", "invalid_params: no position", e)
    e = err(c, "lsp/query", {"kind": "teleport", "path": main_go})
    check(e and e["code"] == -32602 and e["data"]["field"] == "kind", "invalid_params: unknown kind", e)
    e = err(c, "lsp/query", {"kind": "workspaceSymbols"})
    check(e and e["data"]["field"] == "query", "invalid_params: workspaceSymbols without query", e)
    e = err(c, "lsp/query", {"kind": "renamePreview", "path": main_go, "position": {"line": 1, "col": 1}})
    check(e and e["data"]["field"] == "newName", "invalid_params: renamePreview without newName", e)
    log_mark = os.path.getsize(sb.log) if os.path.exists(sb.log) else 0
    e = err(c, "lsp/query", {"kind": "workspaceSymbols", "query": "a", "timeoutMs": 1})
    check(e and e["code"] == 1005, "timeoutMs: timeout", e)
    rid = c.send("lsp/query", {"kind": "references", "path": total_go, "position": {"line": 9, "col": 9}})
    c.notify("$/cancel", {"id": rid})
    msg = c.response(rid)
    check(msg.get("error", {}).get("code") == 1008, "$/cancel: cancelled", msg)
    time.sleep(0.5)
    with open(sb.log, encoding="utf-8", errors="replace") as f:
        f.seek(log_mark)
        log = f.read()
    # Helix logs the server's error reply to a request it cancelled
    check("cancel" in log.lower(), "cancel reaches gopls ($/cancelRequest)", log[-800:])
    r = q(c, "hover", total_go, 6, 7)
    check("func Total" in r["markdown"], "server still fine after cancellations", r)

    # diagnostics: broken.go is not open; gopls publishes for the workspace
    d = wait_diagnostics(c, broken_go, at_least=2)
    if check(d is not None, "diagnostics: published for a file that is not open"):
        check(d["counts"]["errors"] >= 2 and "items" not in d, "diagnostics/get limit 0: counts and total only", d)
        r = c.call("diagnostics/get", {"path": broken_go})
        first = r["items"][0]
        check(first["severity"] == "error" and first["server"] == "gopls" and first["path"] == broken_go, "diagnostics/get: fields", first)
        byline = {i["range"]["start"]["line"]: i for i in r["items"]}
        check(3 in byline and byline[3]["range"]["start"]["col"] == 27, "diagnostics/get: column after multi-byte chars (disk)", byline.get(3))
        w = c.call("diagnostics/get", {})
        check(w["total"] >= r["total"] and any(i["path"] == broken_go for i in w["items"]), "diagnostics/get: workspace", w["counts"])
        w2 = c.call("diagnostics/get", {"limit": 1, "offset": 1})
        check(len(w2["items"]) == 1 and w2["items"][0] == w["items"][1], "diagnostics/get: paging", w2)
        w3 = c.call("diagnostics/get", {"minSeverity": "error"})
        check(all(i["severity"] == "error" for i in w3["items"]) and w3["counts"] == w["counts"], "diagnostics/get: minSeverity", w3["counts"])
        events = [m["params"] for m in c.drain(0.3) if m["method"] == "event/diagnostics"]
        check(any(e["path"] == broken_go and e["errors"] >= 2 for e in events), "event/diagnostics for a file that is not open", events[-3:])
    e = err(c, "diagnostics/get", {"path": P("nope.go")})
    check(e and e["code"] == 1001, "diagnostics/get: not_found", e)
    e = err(c, "diagnostics/get", {"minSeverity": "loud"})
    check(e and e["code"] == -32602, "diagnostics/get: invalid minSeverity", e)

    # event/diagnostics for an open buffer: break main.go in the buffer
    c.drain(0.3)
    hx.esc()
    hx.send("gg")
    hx.send("Ovar zz int = \"🦀\"")
    hx.esc()
    try:
        ev = c.event("event/diagnostics", timeout=20)
        while ev["path"] != main_go:
            ev = c.event("event/diagnostics", timeout=20)
        check(ev["errors"] >= 1, "event/diagnostics: open buffer with an unsaved error", ev)
        d = c.call("diagnostics/get", {"path": main_go})
        check(d["items"] and d["items"][0]["range"]["start"]["line"] == 1, "diagnostics/get: open buffer, unsaved text", d)
    except TimeoutError:
        check(False, "event/diagnostics: open buffer with an unsaved error")
    hx.send("u")

    # --- integration: the server follows editor/reload and editor/applyEdits ---
    def until(f, timeout=20.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                r = f()
                if r:
                    return r
            except RpcError:
                pass
            time.sleep(0.2)
        return None

    def symbols(path):
        return [s["name"] for s in q(c, "documentSymbols", path)["symbols"]]

    c.call("editor/open", {"path": square_go, "focus": False})
    time.sleep(0.05)  # a different mtime from the one Helix read
    sb.write("shapes/square.go", FILES["shapes/square.go"] + "\nfunc (s Square) Perimeter() float64 { return 4 * s.S }\n")
    r = c.call("editor/reload", {"paths": [square_go]})
    check(r["reloaded"] == [square_go], "lsp after reload: the buffer reloaded", r)
    check(until(lambda: any(n.endswith("Perimeter") for n in symbols(square_go))), "lsp after reload: documentSymbols has the new method",
          symbols(square_go))
    h = until(lambda: q(c, "hover", square_go, 7, 17)["markdown"])
    check(h and "Perimeter" in h, "lsp after reload: hover at the new line", h)

    r = c.call("editor/applyEdits", {"path": square_go, "edits": [{
        "range": {"start": {"line": 8, "col": 1}, "end": {"line": 8, "col": 1}},
        "newText": "\nfunc (s Square) Diagonal() float64 { return s.S * 1.4142 }\n"}]})
    check(r["modified"] is True, "lsp after applyEdits: buffer modified, not saved", r)
    check(until(lambda: any(n.endswith("Diagonal") for n in symbols(square_go))), "lsp after applyEdits: documentSymbols has the new method",
          symbols(square_go))
    h = until(lambda: q(c, "hover", square_go, 9, 17)["markdown"])
    check(h and "Diagonal" in h, "lsp after applyEdits: hover at the new line", h)
    with open(square_go) as f:
        check("Diagonal" not in f.read(), "lsp after applyEdits: the file on disk is unchanged")

    # event/lsp: restart gopls
    c.drain(0.2)
    hx.esc()
    hx.send(":lsp-restart" + ENTER)
    try:
        states = []
        deadline = time.monotonic() + 60
        while "initialized" not in states and time.monotonic() < deadline:
            ev = c.event("event/lsp", timeout=60)
            states.append(ev["state"])
            check(ev["name"] == "gopls" and isinstance(ev["id"], int), "event/lsp: id and name", ev) if len(states) == 1 else None
        check(states[:1] == ["exited"] and "initialized" in states, "event/lsp: exited then initialized", states)
    except TimeoutError:
        check(False, "event/lsp after :lsp-restart")
    c.close()


# --- benchmarks ----------------------------------------------------------------


def stats(xs):
    xs = sorted(xs)
    if not xs:
        return {}
    return {"n": len(xs), "p50": round(xs[len(xs) // 2], 3), "p90": round(xs[int(len(xs) * 0.9)], 3),
            "max": round(xs[-1], 3)}


def overhead(c, params, n, gap=0.0):
    """Round trip vs the server's own time (request handed to Helix's client
    until its answer arrived), as the bridge measured it. With `gap`, wait
    that long between queries (an agent asking one thing at a time) instead
    of sending them back to back."""
    rtt, server, issue, shape = [], [], [], []
    for _ in range(5):
        c.call("lsp/query", dict(params, timing=True))
    for _ in range(n):
        if gap:
            time.sleep(gap)
        t = time.perf_counter()
        r = c.call("lsp/query", dict(params, timing=True))
        rtt.append((time.perf_counter() - t) * 1000)
        server.append(r["timing"]["serverMs"])
        issue.append(r["timing"]["issueMs"])
        shape.append(r["timing"]["shapeMs"])
    return {"rtt": stats(rtt), "server": stats(server), "overhead": stats([a - b for a, b in zip(rtt, server)]),
            "issue": stats(issue), "shape": stats(shape)}


def bench(args, Sandbox):
    sb = Sandbox()
    files = dict(FILES)
    # a few thousand diagnostics: one per line, in 20 files
    for k in range(20):
        lines = [f"package many{k}", ""] + [f'var v{i} int = "é{i}"' for i in range(150)]
        files[f"many/m{k}/m.go"] = "\n".join(lines) + "\n"
    make_module(sb, files)
    hx = start(sb, args.hx)
    results = {}
    try:
        reg = sb.registry(hx.pid)
        c = Client(reg["socket"], timeout=60)
        c.call("bridge/hello", {"client": "lspbench"})
        if not wait_server(c):
            print("gopls did not start")
            return 1
        P = sb.path
        time.sleep(2)
        cases = [
            ("definition", {"kind": "definition", "path": P("main.go"), "position": {"line": 9, "col": 40}}),
            ("references", {"kind": "references", "path": P("geo/total.go"), "position": {"line": 6, "col": 7}}),
            ("hover", {"kind": "hover", "path": P("geo/total.go"), "position": {"line": 6, "col": 7}}),
            ("documentSymbols", {"kind": "documentSymbols", "path": P("main.go")}),
            ("workspaceSymbols", {"kind": "workspaceSymbols", "query": "Círculo"}),
            ("implementation", {"kind": "implementation", "path": P("shapes/shape.go"), "position": {"line": 3, "col": 7}}),
            ("incomingCalls", {"kind": "incomingCalls", "path": P("geo/total.go"), "position": {"line": 6, "col": 7}}),
            ("renamePreview", {"kind": "renamePreview", "path": P("geo/total.go"), "position": {"line": 6, "col": 7}, "newName": "Suma"}),
            ("definition (not open, ensure)", {"kind": "definition", "path": P("geo/total.go"), "position": {"line": 9, "col": 11}}),
            ("definition (not open, disk)", {"kind": "definition", "path": P("geo/total.go"), "position": {"line": 9, "col": 11}, "ensure": False}),
        ]
        for label, params in cases:
            r = overhead(c, params, args.n)
            results[label] = r
            print(f"{label:<32} rtt p50 {r['rtt']['p50']:7.3f}  server p50 {r['server']['p50']:7.3f}  "
                  f"overhead p50 {r['overhead']['p50']:.3f} p90 {r['overhead']['p90']:.3f} max {r['overhead']['max']:.3f} ms "
                  f"(issue p50 {r['issue']['p50']:.3f} p90 {r['issue']['p90']:.3f}, shape p50 {r['shape']['p50']:.3f})")
        # diagnostics/get on a few thousand diagnostics
        deadline = time.monotonic() + 120
        total = 0
        while time.monotonic() < deadline:
            total = c.call("diagnostics/get", {"limit": 0})["total"]
            if total >= 3000:
                break
            time.sleep(0.5)
        print(f"diagnostics held: {total}")
        for label, params in (("diagnostics/get limit 0", {"limit": 0}), ("diagnostics/get limit 100", {}),
                              ("diagnostics/get limit 500", {"limit": 500}), ("diagnostics/get one file", {"path": P("many/m3/m.go")}),
                              ("diagnostics/get minSeverity error, offset 2000", {"minSeverity": "error", "offset": 2000})):
            xs = []
            for _ in range(5):
                c.call("diagnostics/get", params)
            for _ in range(args.n):
                t = time.perf_counter()
                c.call("diagnostics/get", params)
                xs.append((time.perf_counter() - t) * 1000)
            results[label] = stats(xs)
            print(f"{label:<46} {results[label]}")
        results["diagnostics_total"] = total
        # main-thread cost of a workspace diagnostics/get: editor/state round trip right behind it
        xs = []
        for _ in range(args.n):
            i = c.send("diagnostics/get", {"limit": 500})
            t = time.perf_counter()
            c.call("editor/state")
            xs.append((time.perf_counter() - t) * 1000)
            c.response(i)
        results["editor/state behind diagnostics/get"] = stats(xs)
        print(f"{'editor/state behind diagnostics/get':<46} {results['editor/state behind diagnostics/get']}")
        results["rss_kib_hx"] = hx.rss_kib()
        c.close()
    finally:
        stop(hx)
        if not args.keep:
            sb.cleanup()
    if args.json:
        with open(args.json, "w") as f:
            json.dump(results, f, indent=2)
    return 0


def big(args, Sandbox):
    """Timings and memory on a large Go repository (`--repo DIR`)."""
    repo = os.path.realpath(args.repo)
    sb = Sandbox()
    env = go_env(sb)
    env["GOFLAGS"] = ""  # the repository's own choice (vendor/ when it has one)
    from live import Hx  # noqa: E402
    target = args.file
    hx = Hx(args.hx, ["-v", target], repo, sb.env(**env))
    results = {}
    try:
        reg = sb.registry(hx.pid, timeout=20)
        if reg is None:
            print("hx did not start:", hx.text_since()[-2000:])
            return 1
        c = Client(reg["socket"], timeout=120)
        c.call("bridge/hello", {"client": "lspbig"})
        t0 = time.monotonic()
        if not wait_server(c, timeout=600):
            print("gopls did not start")
            return 1
        print(f"gopls initialized after {time.monotonic() - t0:.1f}s")
        # wait for the workspace load: the first query blocks until gopls is ready
        t0 = time.monotonic()
        params = json.loads(args.query)
        params.setdefault("kind", "definition")
        params.setdefault("path", os.path.join(repo, target))
        params.setdefault("timeoutMs", 60000)
        for _ in range(20):
            try:
                c.call("lsp/query", params)
                break
            except RpcError as e:
                print("first query:", e)
                time.sleep(2)
        print(f"first query answered after {time.monotonic() - t0:.1f}s")
        if args.settle:
            # gopls keeps type-checking the workspace after its first answer,
            # and its messages are handled on Helix's main thread: wait until
            # it has been idle (under 2% CPU) for 10 s, at most --settle s
            t0 = time.monotonic()
            quiet_since = None
            while time.monotonic() - t0 < args.settle:
                cpu = sum(float(subprocess.run(["ps", "-o", "%cpu=", "-p", str(k)], capture_output=True,
                                               text=True).stdout.strip() or 0) for k in children(hx.pid))
                now = time.monotonic()
                if cpu < 2.0:
                    quiet_since = quiet_since or now
                    if now - quiet_since >= 10:
                        break
                else:
                    quiet_since = None
                time.sleep(1)
            print(f"settled after {time.monotonic() - t0:.0f}s")

        def show(label, r):
            print(f"{label:<28} rtt p50 {r['rtt']['p50']:8.3f}  server p50 {r['server']['p50']:8.3f}  "
                  f"overhead p50 {r['overhead']['p50']:.3f} p90 {r['overhead']['p90']:.3f} max {r['overhead']['max']:.3f} ms "
                  f"(issue p50 {r['issue']['p50']:.3f}, shape p50 {r['shape']['p50']:.3f})")

        for kind in ("definition", "hover", "references", "implementation", "documentSymbols"):
            p = dict(params, kind=kind)
            if kind == "documentSymbols":
                p.pop("position", None)
            for gap in (0.0, 0.1):
                label = kind + (" (100 ms apart)" if gap else " (back to back)")
                try:
                    r = overhead(c, p, args.n, gap)
                except RpcError as e:
                    print(kind, "failed:", e)
                    continue
                results[label] = r
                show(label, r)
        p = {"kind": "workspaceSymbols", "query": args.symbol, "path": params["path"], "timeoutMs": 60000}
        r = overhead(c, p, max(5, args.n // 4), 0.1)
        results["workspaceSymbols (100 ms apart)"] = r
        show("workspaceSymbols (100 ms apart)", r)
        d = c.call("diagnostics/get", {"limit": 0})
        print("diagnostics:", d)
        xs = []
        for _ in range(args.n):
            t = time.perf_counter()
            c.call("diagnostics/get", {"limit": 500})
            xs.append((time.perf_counter() - t) * 1000)
        results["diagnostics/get limit 500"] = stats(xs)
        print("diagnostics/get limit 500", results["diagnostics/get limit 500"])
        kids = children(hx.pid)
        results["rss_kib_hx"] = hx.rss_kib()
        results["rss_kib_gopls"] = sum(int(subprocess.run(["ps", "-o", "rss=", "-p", str(k)], capture_output=True, text=True).stdout.strip() or 0) for k in kids)
        print(f"rss: hx {results['rss_kib_hx'] / 1024:.0f} MiB, gopls {results['rss_kib_gopls'] / 1024:.0f} MiB")
        c.close()
    finally:
        stop(hx, timeout=15)
        sb.cleanup()
    if args.json:
        with open(args.json, "w") as f:
            json.dump(results, f, indent=2)
    return 0


if __name__ == "__main__":
    sys.exit("run through live.py: verify, lspbench or lspbig")
