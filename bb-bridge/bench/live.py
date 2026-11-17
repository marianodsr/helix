#!/usr/bin/env python3
"""Drive a real `hx` under a pty, over its bridge socket.

    live.py verify  [--hx PATH] [--overflow-seconds 70]
    live.py bench   [--hx PATH]
    live.py compat  [--hx PATH] [--config DIR]   # DIR contains helix/config.toml
    live.py lspbench [--hx PATH] [-n 50]          # step 2b, needs ~/go/bin/gopls
    live.py lspbig  --repo DIR --file F --query JSON   # step 2b, a large Go repo

Every run uses a throwaway HOME and XDG directories under $TMPDIR, so the
user's own Helix config, cache and terminals are never touched. `verify`
checks every step 1 method and event (two clients, a client that stops
reading, overflow, disconnect, clean exit, the off switches) and prints
PASS/FAIL lines; `bench` prints round-trip, event latency, memory and
startup numbers; `compat` starts hx with a given config directory, read-only,
and reports anything it complains about.
"""

import argparse
import fcntl
import json
import os
import pty
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
from bridge_client import Client, RpcError  # noqa: E402
from rtt import fmt, measure  # noqa: E402
import live_lsp  # noqa: E402  (step 2b)

ESC = "\x1b"
ENTER = "\r"

BINDINGS = """
[keys.normal.space.b]
s = ":bb-send"
c = ":bb-comment"
C = ":bb-comments-send"
i = ":bb-status"

[keys.select.space.b]
s = ":bb-send"
c = ":bb-comment"
"""


class Hx:
    """hx in a pty; a thread keeps reading its output so it never blocks."""

    def __init__(self, hx, args, cwd, env, cols=120, rows=40):
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(cwd)
            os.execve(hx, [hx] + args, env)
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.out = bytearray()
        self.lock = threading.Lock()
        self.status = None
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        while True:
            try:
                data = os.read(self.fd, 1 << 16)
            except OSError:
                return
            if not data:
                return
            with self.lock:
                self.out += data
                if len(self.out) > 8 << 20:
                    del self.out[: 4 << 20]

    def send(self, keys, pause=0.03):
        os.write(self.fd, keys.encode() if isinstance(keys, str) else keys)
        if pause:
            time.sleep(pause)

    def esc(self):
        # a lone ESC is told apart from an escape sequence by a short wait
        self.send(ESC, pause=0.12)

    def mark(self):
        with self.lock:
            return len(self.out)

    def text_since(self, mark=0):
        with self.lock:
            data = bytes(self.out[mark:])
        text = data.decode("utf-8", "replace")
        return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b[()][A-Z0-9]|\x1b[=>]", "", text)

    def wait_text(self, needle, mark=0, timeout=5.0):
        # Helix only redraws cells that changed, so a space already on screen
        # is skipped (with a cursor move, stripped above): compare without spaces.
        needle = needle.replace(" ", "")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if needle in self.text_since(mark).replace(" ", ""):
                return True
            time.sleep(0.02)
        return False

    def rss_kib(self):
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.pid)], capture_output=True, text=True)
        return int(out.stdout.strip() or 0)

    def wait_exit(self, timeout=5.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = status
                return True
            time.sleep(0.02)
        return False

    def kill(self):
        if self.status is None:
            try:
                os.kill(self.pid, signal.SIGKILL)
                os.waitpid(self.pid, 0)
            except (ProcessLookupError, ChildProcessError):
                pass


class Sandbox:
    """Throwaway HOME, XDG dirs and a small repository."""

    def __init__(self, config_dir=None, bindings=True, extra_config=""):
        base = os.environ.get("TMPDIR", "/tmp")
        if len(base) > 40:
            base = "/tmp"
        self.root = tempfile.mkdtemp(prefix="hxl", dir=base)
        for d in ("home", "cache", "data", "state", "config/helix", "repo/.git", "repo/src"):
            os.makedirs(os.path.join(self.root, d), exist_ok=True)
        if config_dir:
            self.config_home = config_dir
        else:
            self.config_home = os.path.join(self.root, "config")
            with open(os.path.join(self.config_home, "helix", "config.toml"), "w") as f:
                f.write(extra_config + (BINDINGS if bindings else ""))
        self.repo = os.path.join(self.root, "repo")
        self.cache = os.path.join(self.root, "cache")
        self.bridge = os.path.join(self.cache, "helix", "bridge")
        self.log = os.path.join(self.cache, "helix", "helix.log")
        lines = ["package main", "", 'import "fmt"', ""]
        for i in range(2500):
            lines += [f"func f{i}(x int) int {{", f'\tfmt.Println("f{i}", x)', f"\treturn x + {i}", "}", ""]
        self.write("main.go", "\n".join(lines) + "\n")
        self.write("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n")
        self.write("notes.md", "# Notes\n\nnaïve café 🦀 text\n")

    def write(self, rel, text):
        with open(os.path.join(self.repo, rel), "w") as f:
            f.write(text)

    def path(self, rel):
        return os.path.realpath(os.path.join(self.repo, rel))

    def env(self, **extra):
        env = {
            "HOME": os.path.join(self.root, "home"),
            "XDG_CONFIG_HOME": self.config_home,
            "XDG_CACHE_HOME": self.cache,
            "XDG_DATA_HOME": os.path.join(self.root, "data"),
            "XDG_STATE_HOME": os.path.join(self.root, "state"),
            "HELIX_RUNTIME": os.path.join(ROOT, "runtime"),
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            # no language servers or formatters from the user's PATH
            "PATH": "/usr/bin:/bin",
            "LANG": "en_US.UTF-8",
        }
        env.update(extra)
        return env

    def start(self, hx, files=("main.go",), args=("-v",), **env):
        proc = Hx(hx, list(args) + list(files), self.repo, self.env(**env))
        return proc

    def registry(self, pid, timeout=5.0, bridge=None):
        path = os.path.join(bridge or self.bridge, f"{pid}.json")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if os.path.exists(path):
                with open(path) as f:
                    return json.load(f)
            time.sleep(0.01)
        return None

    def cleanup(self):
        shutil.rmtree(self.root, ignore_errors=True)


class Checks:
    def __init__(self):
        self.failed = 0
        self.passed = 0

    def __call__(self, ok, what, detail=""):
        if ok:
            self.passed += 1
            print(f"PASS {what}")
        else:
            self.failed += 1
            print(f"FAIL {what} {detail}")
        sys.stdout.flush()
        return ok


def pos(line, col):
    return {"line": line, "col": col}


def verify(args):
    check = Checks()
    sb = Sandbox()
    hx = sb.start(args.hx, BB_THREAD_ID="thr_live", BB_PROJECT_ID="proj_live")
    try:
        _verify(args, sb, hx, check)
    finally:
        hx.kill()
        if not args.keep:
            sb.cleanup()
    # --- step 2b: diagnostics and lsp (bb-bridge/bench/live_lsp.py) ---
    live_lsp.verify(args, Sandbox, check)
    # --- end step 2b ---
    print(f"\n{check.passed} passed, {check.failed} failed")
    return 1 if check.failed else 0


def _verify(args, sb, hx, check):
    main_go, lib_rs, notes = sb.path("main.go"), sb.path("src/lib.rs"), sb.path("notes.md")
    reg = sb.registry(hx.pid)
    if not check(reg is not None, "registry file written"):
        print(hx.text_since()[-2000:])
        return
    check(reg["protocol"] == 1 and reg["pid"] == hx.pid, "registry protocol and pid")
    check(reg["env"] == {"BB_THREAD_ID": "thr_live", "BB_PROJECT_ID": "proj_live"}, "registry env", reg["env"])
    check(os.path.realpath(reg["workspaceRoot"]) == os.path.realpath(sb.repo), "registry workspaceRoot", reg)
    check(oct(os.stat(reg["socket"]).st_mode & 0o777) == "0o600", "socket is 0600")
    check(oct(os.stat(sb.bridge).st_mode & 0o777) == "0o700", "directory is 0700")

    a = Client(reg["socket"])
    b = Client(reg["socket"])
    hello = a.call("bridge/hello", {"client": "live-a", "clientVersion": "1"})
    b.call("bridge/hello", {"client": "live-b", "clientVersion": "1"})
    check("editor" in hello["capabilities"] and hello["commandPolicy"] == "safe", "hello", hello)

    st = a.call("editor/state")
    check(st["focused"]["path"] == main_go and st["focused"]["languageId"] == "go", "state: focused main.go", st["focused"])
    check(st["mode"] == "normal", "state: mode")

    r = a.call("editor/open", {"path": "src/lib.rs", "line": 2, "col": 5, "endLine": 2, "endCol": 10})
    check(r["path"] == lib_rs and r["deferred"] is False and isinstance(r["viewId"], int), "open relative path", r)
    sel = a.call("editor/selection")
    check(sel["ranges"][0]["text"] == "a + b" and sel["languageId"] == "rust", "selection text", sel)
    r = a.call("editor/open", {"path": notes, "line": 3, "col": 7, "split": "vertical", "note": "bb: look at this"})
    check(hx.wait_text("bb: look at this"), "open note shows on the status line")
    st = a.call("editor/state")
    check(st["focused"]["path"] == notes, "split focused notes.md")
    check(st["focused"]["selections"][0]["start"] == pos(3, 7), "col counts chars (naïve café)", st["focused"]["selections"])
    bg = a.call("editor/open", {"path": main_go, "focus": False, "line": 10})
    st = a.call("editor/state")
    check(st["focused"]["path"] == notes and bg["viewId"] is None, "background open keeps focus")
    t = a.call("editor/text", {"path": "notes.md", "range": {"start": pos(3, 1), "end": pos(3, 11)}})
    check(t["text"] == "naïve café" and t["version"] is not None, "text range of open buffer", t)
    sb.write("other.txt", "on disk only\n")
    t = b.call("editor/text", {"path": "other.txt"})
    check(t["text"] == "on disk only\n" and t["version"] is None, "text from disk", t)
    try:
        a.call("editor/open", {"path": "nope.go"})
        check(False, "open missing file fails")
    except RpcError as e:
        check(e.code == 1001, "open missing file: not_found")
    a.call("editor/status", {"message": "status from live.py", "severity": "error"})
    check(hx.wait_text("status from live.py"), "editor/status shows")

    # events, two clients
    a.call("events/subscribe", {"events": ["selection", "documents", "mode", "send", "diagnostics", "lsp", "externalChange"]})
    b.call("events/subscribe", {"events": ["selection"]})
    a.drain(0.2)
    b.drain(0.2)
    hx.send("j")
    ea, eb = a.event("event/selection"), b.event("event/selection")
    check(ea == eb and ea["path"] == notes and ea["ranges"][0]["start"]["line"] == 4, "selection event to both clients", ea)
    hx.send("i")
    check(a.event("event/mode")["mode"] == "insert", "mode insert")
    hx.send("hello")
    ch = a.event("event/documentChanged")
    check(ch["path"] == notes and ch["modified"] is True, "documentChanged", ch)
    hx.esc()
    check(a.event("event/mode")["mode"] == "normal", "mode normal")
    hx.send(":w" + ENTER)
    sv = a.event("event/documentSaved")
    check(sv["path"] == notes, "documentSaved", sv)
    hx.send(":o src/lib.rs" + ENTER)
    fo = a.event("event/focus")
    check(fo["path"] == lib_rs, "focus event on :o", fo)
    hx.send(":vsplit main.go" + ENTER)
    fo = a.event("event/focus")
    check(fo["path"] == main_go, "focus event on :vsplit", fo)
    hx.send(":o other.txt" + ENTER)
    op = a.event("event/documentOpened")
    check(op["path"] == sb.path("other.txt"), "documentOpened", op)
    hx.send(":bc" + ENTER)
    cl = a.event("event/documentClosed")
    check(cl["path"] == sb.path("other.txt"), "documentClosed", cl)
    check("event/mode" not in [m["method"] for m in b.drain(0.2)], "b gets only what it asked for")

    # :bb-* commands, from the command line and from the user's bindings
    a.drain(0.2)
    hx.send("x:bb-send look here" + ENTER)
    ev = a.event("event/send")
    check(ev["kind"] == "send" and ev["note"] == "look here" and ev["ranges"][0]["text"].endswith("\n"), ":bb-send", ev)
    r = ev["ranges"][0]
    check(r["start"]["col"] == 1 and r["end"] == pos(r["start"]["line"] + 1, 1),
          "line-wise selection ends at column 1 of the next line", r)
    hx.send(":bb-send!" + ENTER)
    check(a.event("event/send")["sendNow"] is True, ":bb-send!")
    hx.send(":bb-ask what does this do?" + ENTER)
    ev = a.event("event/send")
    check(ev["kind"] == "ask" and ev["note"] == "what does this do?", ":bb-ask", ev)
    hx.send(":bb-comment" + ENTER, pause=0.2)
    hx.send("needs a test, 100% \"sure\"" + ENTER)
    ev = a.event("event/send")
    check(ev["kind"] == "comment" and ev["note"] == 'needs a test, 100% "sure"', ":bb-comment prompt", ev)
    hx.send(":bb-comment" + ENTER, pause=0.2)
    hx.send("never mind")
    hx.esc()
    check(not [m for m in a.drain(0.3) if m["method"] == "event/send"], ":bb-comment Escape cancels")
    hx.send(" bc", pause=0.2)
    hx.send("via binding" + ENTER)
    ev = a.event("event/send")
    check(ev["kind"] == "comment" and ev["note"] == "via binding", "space b c binding", ev)
    hx.send("v")
    hx.send("j")
    hx.send(" bs")
    ev = a.event("event/send")
    check(ev["kind"] == "send" and len(ev["ranges"][0]["text"]) > 1, "select-mode binding", ev)
    hx.esc()
    hx.send(" bC")
    check(a.event("event/send")["kind"] == "flush", ":bb-comments-send binding")
    m = hx.mark()
    hx.send(" bi")
    check(hx.wait_text("2 clients", m), ":bb-status shows two clients")

    # an isolated move is reported at once (leading edge): key write -> event
    a.drain(0.2)
    lat = []
    for i in range(30):
        t = time.perf_counter()
        hx.send("j" if i % 2 else "k", pause=0)
        a.event("event/selection")
        lat.append((time.perf_counter() - t) * 1000)
        time.sleep(0.12)
    lat.sort()
    print(f"INFO key -> event/selection, isolated move: p50={lat[len(lat)//2]:.2f}ms max={lat[-1]:.2f}ms")
    check(lat[len(lat) // 2] < 5, "isolated move reported within 5 ms (p50)")

    # a client that stops reading: selection events overflow, it stays
    # move steadily down the big file: every 50 ms window ends somewhere new
    hx.send(":o main.go" + ENTER + "gg")
    slow = Client(reg["socket"])
    slow.call("events/subscribe", {"events": ["selection", "documents"]})
    print(f"INFO slow client: moving the cursor for {args.overflow_seconds}s while it reads nothing")
    end = time.monotonic() + args.overflow_seconds
    i = 0
    rtts = []
    while time.monotonic() < end:
        hx.send("j", pause=0.025)
        i += 1
        if i % 40 == 0:
            t = time.perf_counter()
            a.call("editor/state")
            rtts.append((time.perf_counter() - t) * 1000)
            a.events.clear()
    rtts.sort()
    check(rtts[-1] < 50, "editor stays responsive while a client is stuck", f"max rtt {rtts[-1]:.1f}ms")
    got = slow.drain(1.0)
    overflow = [m for m in got if m.get("method") == "event/overflow"]
    sels = [m for m in got if m.get("method") == "event/selection"]
    check(bool(overflow) and overflow[0]["params"]["dropped"] > 0, "stuck client gets event/overflow",
          f"{len(got)} messages, overflow={overflow[:1]}")
    print(f"INFO slow client: {len(sels)} selection events kept, dropped={overflow[0]['params']['dropped'] if overflow else 0}")
    check(slow.call("editor/state")["mode"] == "normal", "stuck client still served after reading again")

    # a client that never reads responses is disconnected
    flood = Client(reg["socket"])
    closed = False
    try:
        for _ in range(20000):
            flood.send("editor/selection")
    except OSError:
        closed = True
    if not closed:
        try:
            while flood.read(2.0) is not None:
                pass
        except (EOFError, OSError):
            closed = True
    check(closed, "client that never reads is disconnected")
    check(a.call("editor/state")["mode"] == "normal", "others unaffected")

    # protocol errors on a live instance
    raw = Client(reg["socket"])
    raw.send_raw("{oops")
    check(raw.read()["error"]["code"] == -32700, "parse error")
    check(raw.request("annotations/set", {})["error"]["code"] == -32601, "stub method: method_not_found")
    id = raw.send("editor/state")
    raw.notify("$/cancel", {"id": id})
    r = raw.response(id)
    check("result" in r or r["error"]["code"] == 1008, "cancel answered", r)

    # clean exit removes the files
    hx.esc()
    hx.send(":qa!" + ENTER)
    check(hx.wait_exit(), "hx exits")
    check(not os.path.exists(reg["socket"]) and not os.path.exists(os.path.join(sb.bridge, f"{hx.pid}.json")),
          "socket and registry removed on exit")

    # a cache path too long for a socket: the socket moves to /tmp
    sb3 = Sandbox()
    long_cache = os.path.join(sb3.root, "a-long-cache-directory-name/" * 3)
    h3 = sb3.start(args.hx, XDG_CACHE_HOME=long_cache)
    try:
        reg3 = sb3.registry(h3.pid, bridge=os.path.join(long_cache, "helix", "bridge"))
        ok = reg3 is not None and reg3["socket"] == f"/tmp/helix-bridge-{os.getuid()}/{h3.pid}.sock"
        check(ok, "long XDG_CACHE_HOME: socket in /tmp/helix-bridge-<uid>", reg3)
        if ok:
            check(Client(reg3["socket"]).call("bridge/hello")["pid"] == h3.pid, "long XDG_CACHE_HOME: connect")
            check(oct(os.stat(os.path.dirname(reg3["socket"])).st_mode & 0o777) == "0o700", "fallback directory is 0700")
        h3.send(":qa!" + ENTER)
        check(h3.wait_exit() and reg3 and not os.path.exists(reg3["socket"]), "long XDG_CACHE_HOME: socket removed on exit")
    finally:
        h3.kill()
        sb3.cleanup()

    # off switches
    for label, env, extra in (("HELIX_BRIDGE=0", {"HELIX_BRIDGE": "0"}, ""), ("enable = false", {}, "[editor.bridge]\nenable = false\n")):
        sb2 = Sandbox(extra_config=extra)
        h2 = sb2.start(args.hx, **env)
        try:
            time.sleep(1.5)
            check(sb2.registry(h2.pid, timeout=0.1) is None and not os.path.exists(sb2.bridge), f"{label}: no socket")
            h2.send(":qa!" + ENTER)
            h2.wait_exit()
        finally:
            h2.kill()
            sb2.cleanup()


def bench(args):
    sb = Sandbox()
    hx = sb.start(args.hx)
    results = {}
    try:
        reg = sb.registry(hx.pid)
        time.sleep(1.0)
        with open(sb.log) as f:
            log = f.read()
        started = re.search(r"bridge: started in ([0-9.]+)(µs|ms|ns)", log)
        bound = re.search(r"bind and registry took ([0-9.]+)(µs|ms|ns)", log)
        unit = {"ns": 1e-6, "µs": 1e-3, "ms": 1.0}
        if started:
            results["startup_sync_ms"] = float(started.group(1)) * unit[started.group(2)]
        if bound:
            results["startup_bind_ms"] = float(bound.group(1)) * unit[bound.group(2)]

        rss0 = hx.rss_kib()
        idle = []
        for _ in range(10):
            c = Client(reg["socket"])
            c.call("bridge/hello", {"client": "idle"})
            c.call("events/subscribe", {"events": ["selection", "documents", "mode", "send"]})
            idle.append(c)
        time.sleep(2.0)
        rss10 = hx.rss_kib()
        results["rss_kib_no_client"] = rss0
        results["rss_kib_per_idle_client"] = (rss10 - rss0) / 10
        for c in idle:
            c.close()

        c = Client(reg["socket"])
        c.call("bridge/hello", {"client": "bench"})
        c.call("events/subscribe", {"events": ["selection", "documents", "mode", "send"]})
        for method, params, n in (
            ("editor/state", {}, 2000),
            ("editor/selection", {}, 1000),
            ("bridge/hello", {}, 1000),
            ("editor/open", {"path": "main.go", "line": 100}, 1000),
            ("editor/open", {"path": "main.go"}, 1000),
            ("editor/text", {"path": "main.go"}, 200),
        ):
            r = measure(c, method, params, n)
            c.events.clear()
            label = method + (" +line" if params.get("line") else "") + (" (file)" if method == "editor/text" else "")
            r["method"] = label
            results[label] = r
            print(fmt(r))

        def stats(xs):
            xs.sort()
            return {"n": len(xs), "p50": round(xs[len(xs) // 2], 3), "p90": round(xs[int(len(xs) * 0.9)], 3),
                    "p99": round(xs[int(len(xs) * 0.99) - 1], 3), "max": round(xs[-1], 3)}

        # isolated move: sent at once (leading edge); key write -> event
        lat = []
        c.drain(0.2)
        for i in range(200):
            t = time.perf_counter()
            hx.send("j" if i % 2 else "k", pause=0)
            c.event("event/selection")
            lat.append((time.perf_counter() - t) * 1000)
            time.sleep(0.12)
        results["selection_isolated_key_to_event_ms"] = stats(lat)
        # burst of two: the second waits for the window; key write -> second
        # event, minus the 50 ms window (an upper bound on the lateness)
        lat = []
        c.drain(0.2)
        for i in range(200):
            t = time.perf_counter()
            hx.send("kk" if i % 2 else "jj", pause=0)
            c.event("event/selection")
            c.event("event/selection")
            lat.append((time.perf_counter() - t) * 1000 - 50)
            time.sleep(0.12)
        results["selection_burst_after_window_ms"] = stats(lat)
        hx.send(":qa!" + ENTER)
        hx.wait_exit()
    finally:
        hx.kill()
        sb.cleanup()

    # the same, measured inside hx: how late each selection flush runs
    sb = Sandbox()
    hx = sb.start(args.hx, args=("-vv",))
    try:
        reg = sb.registry(hx.pid)
        c = Client(reg["socket"])
        c.call("events/subscribe", {"events": ["selection"]})
        c.drain(0.3)
        for i in range(200):
            hx.send("kk" if i % 2 else "jj", pause=0)
            c.event("event/selection")
            c.event("event/selection")
            time.sleep(0.07)
        hx.send(":qa!" + ENTER)
        hx.wait_exit()
        with open(sb.log) as f:
            inside = [float(v) * {"ns": 1e-6, "µs": 1e-3, "ms": 1.0}[u]
                      for v, u in re.findall(r"selection flushed ([0-9.]+)(ns|µs|ms) after", f.read())]
        inside.sort()
        if inside:
            results["selection_flush_late_ms"] = {
                "n": len(inside), "p50": inside[len(inside) // 2],
                "p99": inside[int(len(inside) * 0.99) - 1], "max": inside[-1],
            }
    finally:
        hx.kill()
        sb.cleanup()
    for k, v in results.items():
        if not (isinstance(v, dict) and "method" in v):
            print(f"{k}: {v}")
    if args.json:
        with open(args.json, "w") as f:
            json.dump(results, f, indent=2)
    return 0


def compat(args):
    """Start hx with a given config dir, read-only, and report complaints."""
    config_home = os.path.realpath(args.config)
    if not os.path.exists(os.path.join(config_home, "helix", "config.toml")):
        sys.exit(f"{config_home}/helix/config.toml not found")
    before = snapshot(config_home)
    sb = Sandbox(config_dir=config_home)
    problems = []
    try:
        env = sb.env()
        health = subprocess.run([args.hx, "--health"], env=env, cwd=sb.repo, capture_output=True, text=True, timeout=60)
        health.stdout, health.stderr = strip_ansi(health.stdout), strip_ansi(health.stderr)
        head = health.stdout.split("\n")[:12]
        print("--health header:\n  " + "\n  ".join(head))
        for line in (health.stdout + health.stderr).split("\n"):
            if re.search(r"(?i)error|invalid|unknown field|failed to (parse|load)", line) and "✘" not in line:
                problems.append(f"--health: {line.strip()}")
        for lang in ("go", "rust", "typescript", "tsx", "json", "toml", "yaml", "markdown", "bash", "python", "starlark", "protobuf", "elixir", "ruby"):
            out = strip_ansi(subprocess.run([args.hx, "--health", lang], env=env, cwd=sb.repo, capture_output=True, text=True, timeout=60).stdout)
            grammar_ok = "Tree-sitter parser: ✓" in out or "Tree-sitter parser: ✔" in out
            hl = re.search(r"Highlight queries: (\S+)", out)
            print(f"  {lang:<11} parser={'ok' if grammar_ok else 'MISSING'} highlights={hl.group(1) if hl else '?'}")
            if not grammar_ok:
                problems.append(f"{lang}: tree-sitter parser not found")

        hx = sb.start(args.hx, files=("main.go", "src/lib.rs", "notes.md"))
        try:
            time.sleep(2.5)
            screen = hx.text_since()
            for pat in ("Bad config", "Bad language config", "Press <ENTER>", "failed to load", "Error", "error:", "Unable", "theme"):
                for m in re.finditer(re.escape(pat), screen):
                    snippet = screen[max(0, m.start() - 80): m.end() + 120].replace("\n", " ")
                    if pat == "theme" and "theme" not in snippet.lower().split("load")[0]:
                        continue
                    problems.append(f"screen: ...{snippet}...")
            reg = sb.registry(hx.pid, timeout=2)
            if reg:
                c = Client(reg["socket"])
                st = c.call("editor/state")
                print(f"started with the config: {len(st['documents'])} documents, mode {st['mode']}")
                c.close()
            else:
                problems.append("bridge did not start under this config")
            # every theme in the config's themes/ directory, as `:theme` loads it
            themes_dir = os.path.join(config_home, "helix", "themes")
            themes = sorted(f[:-5] for f in os.listdir(themes_dir) if f.endswith(".toml")) if os.path.isdir(themes_dir) else []
            for name in themes:
                m = hx.mark()
                hx.send(f":theme {name}" + ENTER, pause=0.25)
                out = hx.text_since(m)
                if re.search(r"(?i)could not|failed|error|invalid|unsupported", out):
                    problems.append(f"theme {name}: {out.strip()[-200:]}")
            print(f"loaded {len(themes)} themes from {themes_dir}")
            hx.send(ESC, pause=0.15)
            hx.send(":qa!" + ENTER)
            hx.wait_exit()
        finally:
            hx.kill()
        if os.path.exists(sb.log):
            with open(sb.log) as f:
                for line in f:
                    if re.search(r"\[(WARN|ERROR)\]", line):
                        problems.append(f"log: {line.strip()}")
    finally:
        sb.cleanup()
    after = snapshot(config_home)
    if before != after:
        problems.append("the config directory changed during the run")
    print("\nproblems:" if problems else "\nno problems: config, languages and themes load without errors or warnings")
    for p in problems:
        print("  " + p)
    return 1 if problems else 0


def strip_ansi(text):
    return re.sub(r"\x1b\[[0-9;]*m", "", text)


def snapshot(directory):
    out = {}
    for base, _, files in os.walk(directory):
        for name in files:
            p = os.path.join(base, name)
            st = os.stat(p)
            out[p] = (st.st_size, st.st_mtime_ns)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mode", choices=["verify", "bench", "compat", "lspbench", "lspbig"])
    ap.add_argument("--hx", default=os.path.join(ROOT, "target", "release", "hx"))
    ap.add_argument("--overflow-seconds", type=float, default=70)
    ap.add_argument("--config", default=os.path.join(ROOT, "bb-bridge", "config"))
    ap.add_argument("--json", help="bench: also write results here")
    ap.add_argument("--keep", action="store_true", help="keep the sandbox directory")
    # step 2b: lspbench (small module), lspbig (a large Go repository)
    ap.add_argument("-n", type=int, default=50, help="lspbench/lspbig: samples per query")
    ap.add_argument("--repo", help="lspbig: repository root")
    ap.add_argument("--file", help="lspbig: file to open, relative to --repo")
    ap.add_argument("--query", default="{}", help="lspbig: lsp/query params (JSON) with a position in --file")
    ap.add_argument("--symbol", default="Handler", help="lspbig: workspaceSymbols query")
    args = ap.parse_args()
    modes = {"verify": verify, "bench": bench, "compat": compat,
             "lspbench": lambda a: live_lsp.bench(a, Sandbox), "lspbig": lambda a: live_lsp.big(a, Sandbox)}
    sys.exit(modes[args.mode](args))


if __name__ == "__main__":
    main()
