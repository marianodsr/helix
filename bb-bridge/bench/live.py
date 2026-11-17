#!/usr/bin/env python3
"""Drive a real `hx` under a pty, over its bridge socket.

    live.py verify  [--hx PATH] [--overflow-seconds 70]
    live.py files   [--hx PATH]                   # only the :bb-files checks
    live.py review  [--hx PATH]                   # only the review checks (step 5)
    live.py watch   [--hx PATH]                   # only the watch checks (step 6)
    live.py bench   [--hx PATH]
    live.py compat  [--hx PATH] [--config DIR]   # DIR contains helix/config.toml
    live.py lspbench [--hx PATH] [-n 50]          # step 2b, needs ~/go/bin/gopls
    live.py lspbig  --repo DIR --file F --query JSON   # step 2b, a large Go repo
    live.py redraw  --before BIN [--hx PATH]      # read-only requests skip the redraw

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
import redraw  # noqa: E402  (integration step)
import live_watch  # noqa: E402  (step 6: the watch)

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

    def text_since_raw(self, mark=0):
        with self.lock:
            return bytes(self.out[mark:]).decode("utf-8", "replace")

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


# --- step 2a: editor/reload, focus check, locations, editor/applyEdits ---------


def _wait(f, timeout=5.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        v = f()
        if v:
            return v
        time.sleep(0.02)
    return None


def _text(c, path):
    return c.call("editor/text", {"path": path})["text"]


def _stall_during(c_socket, action):
    """Run `action` while another client sends editor/state back to back;
    returns (action ms, baseline p50 rtt ms, max rtt ms during the action)."""
    probe = Client(c_socket)
    base = []
    for _ in range(50):
        t = time.perf_counter()
        probe.call("editor/state")
        base.append((time.perf_counter() - t) * 1000)
    base.sort()
    rtts, stop = [], threading.Event()

    def hammer():
        while not stop.is_set():
            t = time.perf_counter()
            probe.call("editor/state")
            rtts.append((time.perf_counter() - t) * 1000)

    th = threading.Thread(target=hammer)
    th.start()
    time.sleep(0.05)
    t = time.perf_counter()
    action()
    took = (time.perf_counter() - t) * 1000
    time.sleep(0.05)
    stop.set()
    th.join()
    probe.close()
    return took, base[len(base) // 2], max(rtts)


def _verify_2a(sb, hx, check, reg):
    # The section before pressed keys: since step 2c a focus-moving request
    # then waits for the user (whenIdle), so these use "always".
    c = Client(reg["socket"])
    caps = c.call("bridge/hello", {"client": "live-2a"})["capabilities"]
    check(all(k in caps for k in ("reload", "locations", "applyEdits")), "2a: capabilities", caps)
    c.call("events/subscribe", {"events": ["externalChange"]})

    # reload keeps the cursor on its line, `u` undoes it
    sb.write("reload.txt", "one\ntwo\nthree\n")
    rl = sb.path("reload.txt")
    c.call("editor/open", {"path": rl, "line": 3, "interrupt": "always"})
    time.sleep(0.02)
    sb.write("reload.txt", "zero\none\ntwo\nthree\n")
    r = c.call("editor/reload", {"paths": [rl]})
    check(r["reloaded"] == [rl] and r["skippedModified"] == [], "2a: reload", r)
    st = c.call("editor/state")
    check(st["focused"]["selections"][0]["start"] == pos(4, 1), "2a: reload keeps the cursor on its line",
          st["focused"]["selections"])
    hx.esc()
    hx.send("u")
    check(_wait(lambda: _text(c, rl) == "one\ntwo\nthree\n"), "2a: u undoes a reload")
    hx.send(":reload" + ENTER)
    _wait(lambda: _text(c, rl) == "zero\none\ntwo\nthree\n")

    # unsaved changes are kept and announced once
    hx.send("ggiX")
    hx.esc()
    time.sleep(0.02)
    sb.write("reload.txt", "from an agent\n")
    r = c.call("editor/reload", {})
    check(rl in r["skippedModified"], "2a: modified buffer skipped", r)
    ev = c.event("event/externalChange", timeout=2)
    check(ev == {"path": rl, "modified": True}, "2a: event/externalChange", ev)
    c.call("editor/reload", {})
    check(not [m for m in c.drain(0.3) if m["method"] == "event/externalChange"], "2a: announced once")
    r = c.call("editor/reload", {"paths": [rl], "force": True})
    check(r["reloaded"] == [rl] and _text(c, rl) == "from an agent\n", "2a: force reload", r)

    # the terminal regains focus: clean buffers reload, modified ones warn
    sb.write("focus.txt", "before\n")
    fp = sb.path("focus.txt")
    c.call("editor/open", {"path": fp, "focus": False})
    time.sleep(0.02)
    sb.write("focus.txt", "after\n")
    hx.send("\x1b[O")
    hx.send("\x1b[I")
    check(_wait(lambda: _text(c, fp) == "after\n"), "2a: focus-in reloads a clean buffer")
    hx.send("ggiY")
    hx.esc()
    time.sleep(0.02)
    sb.write("reload.txt", "again\n")
    m = hx.mark()
    hx.send("\x1b[I")
    ev = c.event("event/externalChange", timeout=2)
    check(ev["path"] == rl, "2a: focus-in announces a modified buffer", ev)
    check(hx.wait_text("changed on disk", m), "2a: focus-in warns on the status line")
    check(_text(c, rl).startswith("Y"), "2a: the user's edit is kept")
    hx.send(":reload" + ENTER)

    # locations: goto, walk with wrap-around, picker
    lib_rs = sb.path("src/lib.rs")
    items = [
        {"path": "src/lib.rs", "line": 2, "label": "body", "note": "the sum"},
        {"path": "reload.txt", "line": 1, "note": "reloaded file"},
        {"path": "gone.txt", "line": 1, "note": "deleted"},
    ]
    m = hx.mark()
    r = c.call("locations/set", {"list": "tour", "title": "Live tour", "items": items, "goto": 0, "interrupt": "always"})
    check(r == {"count": 3, "deferred": False}, "2a: locations/set", r)
    check(c.call("editor/state")["focused"]["path"] == lib_rs, "2a: goto jumps")
    check(hx.wait_text("tour 1/3: the sum", m), "2a: goto shows the note")
    m = hx.mark()
    hx.send(":bb-next" + ENTER)
    check(hx.wait_text("tour 2/3: reloaded file", m), "2a: :bb-next")
    m = hx.mark()
    hx.send(":bb-next" + ENTER)
    check(hx.wait_text("tour 1/3: the sum (skipped 1 missing file, wrapped)", m), "2a: :bb-next skips and wraps")
    m = hx.mark()
    hx.send("j")
    hx.send("k")
    check(hx.wait_text("tour 1/3: the sum", m), "2a: the note stays while the cursor is on the item")
    hx.send(":bb-prev" + ENTER)
    _wait(lambda: c.call("editor/state")["focused"]["path"] == rl)
    m = hx.mark()
    hx.send(":bb-locations" + ENTER)
    check(hx.wait_text("reloaded file", m) and hx.wait_text("body", m), "2a: :bb-locations picker shows the items")
    time.sleep(0.2)
    hx.send(ENTER)
    check(_wait(lambda: c.call("editor/state")["focused"]["path"] == lib_rs), "2a: picker jumps to the item")
    c.call("locations/clear", {})

    # applyEdits: one undo step, version check, background open and save
    v = c.call("editor/state")["focused"]["version"]
    r = c.call("editor/applyEdits", {"path": lib_rs, "version": v, "edits": [
        {"range": {"start": pos(2, 5), "end": pos(2, 10)}, "newText": "a - b"},
        {"range": {"start": pos(1, 8), "end": pos(1, 11)}, "newText": "sub"},
    ]})
    check(r["version"] > v and r["modified"] is True, "2a: applyEdits", r)
    check(_text(c, lib_rs) == "pub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n", "2a: applyEdits text")
    try:
        c.call("editor/applyEdits", {"path": lib_rs, "version": v, "edits": []})
        check(False, "2a: stale version refused")
    except RpcError as e:
        check(e.code == 1002 and e.error["data"]["version"] == r["version"], "2a: stale version refused", e.error)
    hx.esc()
    hx.send("u")
    check(_wait(lambda: _text(c, lib_rs) == "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n"),
          "2a: one u restores the buffer")
    sb.write("bg.txt", "background\n")
    bg = sb.path("bg.txt")
    r = c.call("editor/applyEdits", {"path": "bg.txt", "save": True, "edits": [
        {"range": {"start": pos(1, 1), "end": pos(1, 1)}, "newText": "saved "}]})
    with open(bg) as f:
        on_disk = f.read()
    check(r["modified"] is False and on_disk == "saved background\n", "2a: background applyEdits with save", r)
    check(c.call("editor/state")["focused"]["path"] == lib_rs, "2a: background edit keeps focus")

    # a 20k-line file: how long does a reload hold the main thread?
    lines = ["package main", ""]
    for i in range(4000):
        lines += [f"func g{i}(x int) int {{", f"\treturn x * {i}", "}", "", f"// g{i} ends"]
    big = "\n".join(lines[:20000]) + "\n"
    sb.write("big.go", big)
    big_go = sb.path("big.go")
    c.call("editor/open", {"path": big_go, "line": 10000, "interrupt": "always"})
    time.sleep(0.3)

    def rewrite(text):
        time.sleep(0.01)
        sb.write("big.go", text)

    def bridge_reload():
        assert c.call("editor/reload", {"paths": [big_go]})["reloaded"] == [big_go]

    def helix_reload():
        hx.send(":reload" + ENTER, pause=0)
        _wait(lambda: _text(c, big_go) == big, timeout=10)

    few = big.replace("\treturn x * 1", "\treturn x + 1")  # 1111 lines
    every = "\n".join(line + " " for line in big.split("\n"))  # every line
    for label, text in (("1111 of 20000 lines changed", few), ("every line changed", every)):
        rewrite(text)
        took, p50, ours = _stall_during(reg["socket"], bridge_reload)
        print(f"INFO editor/reload of 20k lines, {label}: request {took:.1f}ms, "
              f"editor/state p50 {p50:.2f}ms idle, worst {ours:.1f}ms during the reload")
        # the same change back with Helix's own :reload, which reads and
        # diffs on the main thread; applying the change (tree-sitter, redraw)
        # costs both the same
        rewrite(big)
        took, p50, helix = _stall_during(reg["socket"], helix_reload)
        print(f"INFO Helix's own :reload of the same change: {took:.1f}ms, "
              f"worst editor/state {helix:.1f}ms during it")
        check(ours <= helix + 10, f"2a: reload of 20k lines ({label}) stalls no more than Helix's :reload",
              f"bridge {ours:.1f}ms, :reload {helix:.1f}ms")
    hx.send(":bc!" + ENTER)
    c.close()


def verify(args):
    check = Checks()
    # the step 1 and 2a checks rewrite files and drive editor/reload
    # themselves; the watch has its own section (live_watch.py)
    sb = Sandbox(extra_config="[editor.bridge]\nwatch = false\n")
    hx = sb.start(args.hx, BB_THREAD_ID="thr_live", BB_PROJECT_ID="proj_live")
    try:
        _verify(args, sb, hx, check)
    finally:
        hx.kill()
        if not args.keep:
            sb.cleanup()
    _verify_2c(args, check)
    verify_annotations_reveal(args, check)
    verify_files(args, check)
    live_watch.verify(args, Sandbox, check)
    verify_review(args, check)
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
    check(hello["capabilities"] == ["editor", "reload", "locations", "views", "command", "diagnostics", "lsp",
                                    "applyEdits", "annotations", "reveal", "review"] and hello["commandPolicy"] == "safe",
          "hello: every capability, policy safe", hello)

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

    def moving(seconds):
        """Move the cursor for `seconds`; a's editor/state round trips meanwhile."""
        end = time.monotonic() + seconds
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
        return rtts

    # the same load without the stuck client first, as a baseline from this
    # run (a loaded machine is slow in both)
    base = moving(min(10.0, args.overflow_seconds))
    slow = Client(reg["socket"])
    slow.call("events/subscribe", {"events": ["selection", "documents"]})
    print(f"INFO slow client: moving the cursor for {args.overflow_seconds}s while it reads nothing")
    rtts = moving(args.overflow_seconds)
    p90 = lambda xs: xs[int(len(xs) * 0.9)]
    print(f"INFO editor/state while moving: p90 {p90(base):.2f}ms alone, {p90(rtts):.2f}ms with the stuck client"
          f" (max {base[-1]:.1f}ms, {rtts[-1]:.1f}ms)")
    check(p90(rtts) < 3 * p90(base) + 5 and rtts[-1] < 500, "editor stays responsive while a client is stuck",
          f"p90 {p90(rtts):.1f}ms vs {p90(base):.1f}ms alone, max {rtts[-1]:.1f}ms")
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

    _verify_2a(sb, hx, check, reg)

    # protocol errors on a live instance
    raw = Client(reg["socket"])
    raw.send_raw("{oops")
    check(raw.read()["error"]["code"] == -32700, "parse error")
    check(raw.request("no/such/method", {})["error"]["code"] == -32601, "unknown method: method_not_found")
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

    # integration step: HELIX_KITTY_KEYBOARD overrides kitty-keyboard-protocol
    # at start (the pty here answers no queries, so auto never turns it on)
    for value, push, query in ((None, False, True), ("enabled", True, False), ("disabled", False, False)):
        sb4 = Sandbox()
        h4 = sb4.start(args.hx, **({"HELIX_KITTY_KEYBOARD": value} if value else {}))
        try:
            started = sb4.registry(h4.pid) is not None
            time.sleep(0.3)
            with h4.lock:
                raw = bytes(h4.out)
            pushed = re.search(rb"\x1b\[>[0-9]+u", raw) is not None
            queried = b"\x1b[?u" in raw
            check(started and pushed == push and queried == query,
                  f"HELIX_KITTY_KEYBOARD={value or '(unset)'}: kitty keyboard {'pushed' if push else 'not pushed'}"
                  f"{', queried' if query else ', not queried'}", f"pushed={pushed} queried={queried}")
            h4.send(":qa!" + ENTER)
            h4.wait_exit()
        finally:
            h4.kill()
            sb4.cleanup()

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


# --- step 2c: panes, selections, editor/command and its policy, not yanking ---


def _verify_2c(args, check):
    sb = Sandbox()
    marker = os.path.join(sb.root, "pwned")
    with open(os.path.join(sb.config_home, "helix", "config.toml"), "a") as f:
        f.write(f'\n[keys.normal]\nX = ":sh touch {marker}"\nY = "shell_pipe"\n')
    hx = sb.start(args.hx, files=("main.go",))
    try:
        reg = sb.registry(hx.pid)
        if not check(reg is not None, "2c: registry file written"):
            return
        main_go, notes = sb.path("main.go"), sb.path("notes.md")
        c = Client(reg["socket"])
        hello = c.call("bridge/hello", {"client": "live-2c"})
        check({"views", "command"} <= set(hello["capabilities"]), "2c: hello lists views and command", hello)
        now = {"interrupt": "always"}

        # whole-line ranges: endLine without endCol takes the lines whole, as `x` does
        sb.write("lines.txt", "first\nnaïve café 🦀\nthird\nfour line, 15ch\nlast\n")
        sb.write("bare.txt", "first\nnaïve café 🦀\nlast")
        for rel, line, end, want in (("lines.txt", 3, 5, "third\nfour line, 15ch\nlast\n"), ("lines.txt", 1, 1, "first\n"),
                                     ("lines.txt", 2, 2, "naïve café 🦀\n"), ("lines.txt", 5, 5, "last\n"),
                                     ("bare.txt", 3, 3, "last"), ("bare.txt", 2, 3, "naïve café 🦀\nlast")):
            c.call("editor/open", {"path": rel, "line": line, "endLine": end, **now})
            got = c.call("editor/selection")["ranges"][0]["text"]
            check(got == want, f"2c: {rel} lines {line}-{end} whole", repr(got))
        c.call("editor/select", {"ranges": [{"start": pos(2, 1), "end": {"line": 2, "col": 2**53}}], **now})
        got = c.call("editor/selection")["ranges"][0]["text"]
        check(got == "naïve café 🦀", "2c: a huge column clamps to the line end", repr(got))
        c.call("editor/open", {"path": main_go, **now})
        m = hx.mark()
        hx.send(":bb-status" + ENTER, pause=0.2)
        check(hx.wait_text("offers all of protocol 1", m), "2c: :bb-status: every capability offered")

        # panes and selections
        lst = c.call("view/list")
        first = lst["views"][0]["viewId"]
        check(len(lst["views"]) == 1 and lst["layout"] == {"split": "vertical", "children": [{"viewId": first}]},
              "2c: view/list, one pane", lst)
        r = c.call("editor/open", {"path": "notes.md", "split": "vertical", **now})
        lst = c.call("view/list")
        check(lst["layout"] == {"split": "vertical", "children": [{"viewId": first}, {"viewId": r["viewId"]}]}
              and lst["views"][1]["focused"] and lst["views"][1]["area"]["x"] > 0, "2c: vsplit in the layout", lst)
        c.call("view/focus", {"viewId": first, **now})
        check(c.call("editor/state")["focused"]["path"] == main_go, "2c: view/focus")
        c.call("editor/select", {"ranges": [{"start": pos(1, 1), "end": pos(1, 8)}, {"start": pos(3, 1), "end": pos(3, 7)}],
                                 "primary": 1, **now})
        sel = c.call("editor/selection")
        check([x["text"] for x in sel["ranges"]] == ["package", "import"] and sel["primary"] == 1, "2c: editor/select", sel)
        c.call("view/close", {"viewId": r["viewId"]})
        check(len(c.call("view/list")["views"]) == 1, "2c: view/close")

        # editor/command
        r = c.call("editor/command", {"typed": "echo hello from 2c", **now})
        check(r["status"]["message"] == "hello from 2c" and hx.wait_text("hello from 2c"), "2c: typed, status shown", r)
        r = c.call("editor/command", {"static": "goto_line_end", **now})
        check(r["deferred"] is False and r["mode"] == "normal", "2c: static", r)
        r = c.call("editor/command", {"keys": "<space>f", **now})
        check(r.get("overlay") == "picker", "2c: keys open the picker", r)
        r = c.call("editor/command", {"keys": "<esc>", **now})
        check("overlay" not in r, "2c: <esc> closes it", r)

        # the safe policy, by every route
        for params in ({"typed": f"sh touch {marker}"}, {"typed": f"echo %sh{{touch {marker}}}"},
                       {"typed": f'echo "%sh{{touch {marker}}}"'}, {"typed": "set bridge.commands all"},
                       {"static": "shell_pipe"}, {"keys": "X"}, {"keys": "Y"}, {"keys": f"|touch {marker}<ret>"},
                       {"keys": f":sh touch {marker}<ret>"}, {"keys": "<space>?"}):
            try:
                c.call("editor/command", {**params, **now})
                check(False, f"2c: refused {params}")
            except RpcError as e:
                check(e.code == 1007, f"2c: refused {params}", e)
        time.sleep(0.5)
        check(not os.path.exists(marker), "2c: no shell ran")
        c.call("editor/command", {"keys": "ihello<esc>", **now})
        try:
            c.call("editor/command", {"typed": "q!", **now})
            check(False, "2c: q! refused")
        except RpcError as e:
            check(e.code == 1007, "2c: q! refused on a modified buffer")
        check(c.call("editor/state")["focused"]["modified"] is True, "2c: unsaved work kept")

        # not yanking the user
        time.sleep(1.6)
        r = c.call("editor/open", {"path": notes})
        check(r["deferred"] is False, "2c: idle user: open moves at once", r)
        hx.send("j", pause=0.2)
        r = c.call("editor/open", {"path": main_go, "line": 40})
        check(r["deferred"] is True and c.call("editor/state")["focused"]["path"] == notes, "2c: typing user: deferred", r)
        check(hx.wait_text("bb wants to show main.go:40 (:bb-go)"), "2c: notice on the status line")
        m = hx.mark()
        hx.send("k", pause=0.2)
        check(hx.wait_text("bb wants to show main.go:40", m, timeout=4), "2c: notice shown again after a pause")
        hx.send(":bb-go" + ENTER, pause=0.3)
        st = c.call("editor/state")
        check(st["focused"]["path"] == main_go and st["focused"]["selections"][0]["start"]["line"] == 40,
              "2c: :bb-go performs it", st["focused"])
        hx.send("i", pause=0.2)
        r = c.call("view/focus", {"viewId": first})
        check(r["deferred"] is False, "2c: focusing the focused pane never waits", r)
        r = c.call("editor/command", {"typed": "vsplit"})
        check(r == {"deferred": True}, "2c: insert mode: command deferred", r)
        hx.esc()
        hx.send(":qa!" + ENTER)
        check(hx.wait_exit(), "2c: hx exits")
    finally:
        hx.kill()
        if not args.keep:
            sb.cleanup()


# --- :bb-files: the file manager as a chooser, with no client ---------------------

FILES_STUB = r"""#!/bin/sh
# A stand-in for yazi: logs how it was started and the terminal it got,
# reads one line from the terminal, then writes what $SB/mode says.
chooser="$1"; start="$2"; sb="$(dirname "$0")"
log="$sb/stub.log"
echo "args: $chooser | $start" > "$log"
[ -t 0 ] && echo "stdin: tty" >> "$log"
[ -t 1 ] && echo "stdout: tty" >> "$log"
echo "stty: $(stty -a 2>&1 | tr '\n' ' ')" >> "$log"
printf 'STUB-SCREEN-UP\n'
IFS= read -r line
echo "read: $line" >> "$log"
case "$(cat "$sb/mode")" in
  one) printf '%s\n' "$sb/repo/notes.md" > "$chooser" ;;
  two) printf '%s\n%s\n' "$sb/repo/src/lib.rs" "$sb/repo/notes.md" > "$chooser" ;;
  none) ;;
esac
echo done >> "$log"
"""

FILES_BINDINGS = """
[keys.normal.space]
e = ":bb-files"

[keys.select.space]
e = ":bb-files"
"""


def verify_files(args, check):
    sb = Sandbox(bindings=False)
    try:
        stub = os.path.join(sb.root, "fm.sh")
        with open(stub, "w") as f:
            f.write(FILES_STUB)
        os.chmod(stub, 0o755)
        with open(os.path.join(sb.config_home, "helix", "config.toml"), "w") as f:
            f.write(f'[editor.bridge]\nfile-manager = ["{stub}", "{{chooser}}", "{{path}}"]\n' + FILES_BINDINGS)
        hx = sb.start(args.hx)
        try:
            _verify_files(sb, hx, check)
        finally:
            hx.kill()
    finally:
        if not args.keep:
            sb.cleanup()
    # a program that is not there: an error on the status line, Helix goes on
    sb = Sandbox(bindings=False)
    try:
        with open(os.path.join(sb.config_home, "helix", "config.toml"), "w") as f:
            f.write('[editor.bridge]\nfile-manager = ["helix-bb-missing-fm", "{chooser}"]\n' + FILES_BINDINGS)
        hx = sb.start(args.hx)
        try:
            reg = sb.registry(hx.pid)
            c = Client(reg["socket"])
            c.call("bridge/hello", {"client": "live-files"})
            time.sleep(0.3)
            m = hx.mark()
            hx.send(" e", pause=0.3)
            check(hx.wait_text("helix-bb-missing-fm not found", m), "files: missing program is a status-line error")
            check("\x1b[?1049l" not in hx.text_since_raw(m), "files: missing program leaves the screen alone")
            hx.send("i", pause=0.2)
            check(c.call("editor/state", {})["mode"] == "insert", "files: Helix takes keys after the error")
            hx.esc()
        finally:
            hx.kill()
    finally:
        if not args.keep:
            sb.cleanup()


def watch_only(args):
    check = Checks()
    live_watch.verify(args, Sandbox, check)
    print(f"\n{check.passed} passed, {check.failed} failed")
    return 1 if check.failed else 0


def verify_files_only(args):
    check = Checks()
    verify_files(args, check)
    print(f"\n{check.passed} passed, {check.failed} failed")
    return 1 if check.failed else 0


def _last(raw, on, off):
    """Whether the last of two escape sequences in `raw` is `on`."""
    return raw.rfind(on) > raw.rfind(off)


def _verify_files(sb, hx, check):
    reg = sb.registry(hx.pid)
    if not check(reg is not None, "files: registry file written"):
        return
    c = Client(reg["socket"])
    c.call("bridge/hello", {"client": "live-files"})
    main_go, notes, lib_rs = sb.path("main.go"), sb.path("notes.md"), sb.path("src/lib.rs")
    real = os.path.realpath
    log = os.path.join(sb.root, "stub.log")
    mode = os.path.join(sb.root, "mode")
    time.sleep(0.4)

    def run(keys, which):
        with open(mode, "w") as f:
            f.write(which)
        if os.path.exists(log):
            os.remove(log)
        m = hx.mark()
        hx.send(keys, pause=0.1)
        up = hx.wait_text("STUB-SCREEN-UP", m, timeout=5)
        # typed while the file manager runs: it must get the line, not Helix
        hx.send("picked\r", pause=0.1)
        _wait(lambda: os.path.exists(log) and "done" in open(log).read())
        time.sleep(0.4)
        return m, up, open(log).read() if os.path.exists(log) else ""

    before = c.call("editor/state", {})
    m, up, text = run(" e", "one")
    check(up, "files: space e runs the file manager full screen")
    check("stdin: tty" in text and "stdout: tty" in text, "files: the program is attached to the terminal", text)
    check(" icanon" in text and " echo" in text, "files: the terminal is in cooked mode for it", text)
    check("read: picked" in text, "files: keys typed go to the program, not to Helix", text)
    args_line = next((l for l in text.splitlines() if l.startswith("args: ")), "")
    chooser, _, start = args_line[len("args: "):].partition(" | ")
    check(real(start) == main_go, "files: {path} is the focused file", args_line)
    check(chooser != "" and not os.path.exists(chooser), "files: the chooser file is removed", chooser)
    raw = hx.text_since_raw(m)
    left, came_back = raw.find("\x1b[?1049l"), raw.rfind("\x1b[?1049h")
    stub_at = raw.find("STUB-SCREEN-UP")
    check(0 <= left < stub_at < came_back, "files: main screen while it runs, alternate screen after",
          (left, stub_at, came_back))
    check(raw.find("\x1b[?1006l") < stub_at and _last(raw, "\x1b[?1006h", "\x1b[?1006l"),
          "files: mouse capture off while it runs, on after")
    check(_last(raw, "\x1b[?2004h", "\x1b[?2004l"), "files: bracketed paste back on")
    st = c.call("editor/state", {})
    check(real(st["focused"]["path"]) == notes, "files: the chosen file is open and focused", st["focused"]["path"])
    check(hx.wait_text("notes.md", m), "files: the editor is drawn again")
    hx.send("i", pause=0.2)
    check(c.call("editor/state", {})["mode"] == "insert", "files: Helix takes keys again afterwards")
    hx.esc()

    docs = len(c.call("editor/state", {})["documents"])
    m, up, text = run("v e", "two")
    st = c.call("editor/state", {})
    paths = [real(d["path"]) for d in st["documents"]]
    check(up and {lib_rs, notes} <= set(paths) and len(paths) == len(set(paths)),
          "files: two chosen files open (select mode), no duplicate buffer", paths)
    check(real(st["focused"]["path"]) == lib_rs, "files: the first chosen file is focused", st["focused"]["path"])
    hx.esc()

    focused = st["focused"]["path"]
    docs = len(st["documents"])
    m, up, text = run(" e", "none")
    st = c.call("editor/state", {})
    check(up and st["focused"]["path"] == focused and len(st["documents"]) == docs,
          "files: nothing chosen changes nothing")
    raw = hx.text_since_raw(m)
    check(_last(raw, "\x1b[?1049h", "\x1b[?1049l") and _last(raw, "\x1b[?1006h", "\x1b[?1006l"),
          "files: the screen is back after a cancel")
    check(before["focused"]["path"] == main_go, "files: started on main.go")

    # with bb listening, only the event
    c2 = Client(reg["socket"])
    c2.call("events/subscribe", {"events": ["files"]})
    with open(mode, "w") as f:
        f.write("one")
    if os.path.exists(log):
        os.remove(log)
    m = hx.mark()
    hx.send(" e", pause=0.3)
    try:
        ev = c2.event("event/files", timeout=3)
    except Exception as err:  # noqa: BLE001
        ev = {"error": str(err)}
    time.sleep(0.5)
    check(ev.get("path") == focused, "files: with a client subscribed, event/files with the focused file", ev)
    check(not os.path.exists(log), "files: with a client subscribed the program does not run")


# --- step 5: review/set, the inline diff, :bb-review, :bb-diff ---------------------


def verify_review_only(args):
    check = Checks()
    verify_review(args, check)
    print(f"\n{check.passed} passed, {check.failed} failed")
    return 1 if check.failed else 0


def _git(repo, *args):
    env = dict(os.environ, GIT_AUTHOR_NAME="a", GIT_AUTHOR_EMAIL="a@example.com",
               GIT_COMMITTER_NAME="c", GIT_COMMITTER_EMAIL="c@example.com")
    env.pop("GIT_DIR", None)
    return subprocess.run(["git", "-C", repo, *args], env=env, check=True,
                          capture_output=True, text=True).stdout.strip()


def verify_review(args, check):
    sb = Sandbox(bindings=False)
    repo = sb.repo
    shutil.rmtree(os.path.join(repo, ".git"), ignore_errors=True)
    _git(repo, "init", "-q")
    text = "".join(f"line {i}\n" for i in range(1, 41))
    sb.write("review.txt", text.replace("line 5\n", "line 5 LIVE-REMOVED-TEXT\n"))
    sb.write("gone.txt", "LIVE-GONE-TEXT\n")
    _git(repo, "add", "-A")
    _git(repo, "-c", "commit.gpgsign=false", "commit", "-qm", "base")
    base = _git(repo, "rev-parse", "HEAD")
    sb.write("review.txt", text.replace("line 5\n", "line 5 LIVE-NEW-TEXT\n"))
    os.remove(os.path.join(repo, "gone.txt"))
    hx = sb.start(args.hx, files=("review.txt",))
    try:
        _verify_review(sb, hx, check, base)
    finally:
        hx.kill()
        if not args.keep:
            sb.cleanup()


def _check_comment_anchoring(hx, c, check, label):
    """`x` three times from line 10, and a range inside line 11, then
    `:bb-comment`: event/send carries exactly those ranges (review.txt holds
    `line N` on line N)."""
    hx.esc()
    hx.send(";", pause=0.05)
    c.drain(0.2)
    hx.send("10gg", pause=0.1)
    hx.send("xxx", pause=0.15)
    hx.send(":bb-comment three lines" + ENTER, pause=0.2)
    ev = c.event("event/send", timeout=3)
    r = ev.get("ranges", [])
    check(len(r) == 1 and r[0]["start"] == {"line": 10, "col": 1} and r[0]["end"] == {"line": 13, "col": 1}
          and r[0]["text"] == "line 10\nline 11\nline 12\n" and ev.get("note") == "three lines",
          f"{label}: x on lines 10-12: the three whole lines (to 13:1)", r)
    hx.esc()
    hx.send(";", pause=0.05)
    # inside one line: from col 3, three characters
    hx.send("11gglll", pause=0.15)
    hx.send("vll", pause=0.15)
    sel = c.call("editor/selection")["ranges"]
    hx.send(":bb-comment part of a line" + ENTER, pause=0.2)
    ev = c.event("event/send", timeout=3)
    r = ev.get("ranges", [])
    check(len(r) == 1 and r[0]["start"] == {"line": 11, "col": 4} and r[0]["end"] == {"line": 11, "col": 7}
          and r[0]["text"] == "e 1" and r == sel,
          f"{label}: a range inside line 11: exactly those characters", (r, sel))
    hx.esc()
    hx.esc()
    hx.send(";gg", pause=0.1)


def _verify_review(sb, hx, check, base):
    path, gone = sb.path("review.txt"), os.path.join(sb.repo, "gone.txt")
    reg = sb.registry(hx.pid)
    if not check(reg is not None, "review: registry file written"):
        return
    c = Client(reg["socket"])
    hello = c.call("bridge/hello", {"client": "live-review"})
    check("review" in hello["capabilities"], "review: capability", hello["capabilities"])
    c.call("events/subscribe", {"events": ["send"]})
    time.sleep(0.3)
    _check_comment_anchoring(hx, c, check, "comment anchoring, no review")
    m = hx.mark()
    r = c.call("review/set", {"id": "live", "title": "Live", "base": {"rev": base}, "files": [
        {"path": path, "status": "modified", "additions": 1, "deletions": 1},
        {"path": gone, "status": "deleted", "additions": 0, "deletions": 1},
    ]})
    check(r == {"files": 2, "base": base, "deferred": False}, "review: review/set result", r)
    check(hx.wait_text("line 5 LIVE-REMOVED-TEXT", m), "review: the removed line is drawn above the change")
    st = c.call("review/state", {})
    check(st["active"] and st["inline"] and st["files"] == 2, "review: review/state", st)
    _check_comment_anchoring(hx, c, check, "comment anchoring, during a review")
    t = c.call("editor/text", {"path": path})
    check("LIVE-REMOVED-TEXT" not in t["text"] and not t["modified"], "review: the removed line is not in the document")

    # typing while reviewing: lands in the document, undo works
    hx.send("ggOLIVE-TYPED", pause=0.1)
    hx.esc()
    t = c.call("editor/text", {"path": path})
    check(t["text"].startswith("LIVE-TYPED\nline 1\n"), "review: typed text goes into the document", t["text"][:40])
    hx.send("u", pause=0.3)
    t = c.call("editor/text", {"path": path})
    check(t["text"].startswith("line 1\n") and not t["modified"], "review: undo")

    # comments carry the review
    hx.send(":bb-comment about this" + ENTER, pause=0.2)
    ev = c.event("event/send", timeout=3)
    check(ev.get("reviewId") == "live", "review: event/send carries reviewId", ev)

    # :bb-review takes free text: every word goes to the client
    c.call("events/subscribe", {"events": ["review", "send"]})
    hx.send(":bb-review commit HEAD~1" + ENTER, pause=0.2)
    ev = c.event("event/review", timeout=3)
    check(ev.get("target") == "commit HEAD~1", "review: :bb-review commit HEAD~1 sends the whole target", ev)

    # the picker: both files, with counts
    m = hx.mark()
    hx.send(":bb-review" + ENTER, pause=0.5)
    check(hx.wait_text("+1 -1", m) and hx.wait_text("gone.txt", m), "review: :bb-review lists the files")
    check(hx.wait_text("-line 5 LIVE-REMOVED-TEXT", m) or hx.wait_text("- line 5 LIVE-REMOVED-TEXT", m),
          "review: the preview shows the diff")
    m = hx.mark()
    hx.send("gone" + ENTER, pause=0.6)
    st = c.call("editor/state", {})
    check(st["focused"]["path"] is None and hx.wait_text("gone.txt (deleted)", m),
          "review: a deleted file opens as a scratch buffer named on the status line", st["focused"]["path"])
    t = c.call("editor/text", {"docId": st["focused"]["docId"]})
    check(t["text"] == "LIVE-GONE-TEXT\n", "review: with its text at the base", t["text"])
    c.call("editor/open", {"path": path, "interrupt": "always"})

    # :bb-diff hides and shows it
    m = hx.mark()
    hx.send(":bb-diff" + ENTER, pause=0.3)
    check(hx.wait_text("Inline diff off", m), "review: :bb-diff off")
    check(c.call("review/state", {})["inline"] is False, "review: review/state follows :bb-diff")
    m = hx.mark()
    hx.send(":bb-diff" + ENTER, pause=0.3)
    check(hx.wait_text("Inline diff on (Live)", m), "review: :bb-diff on")

    # errors and the end
    try:
        c.call("review/set", {"id": "x", "base": {"rev": "0" * 40}, "files": []})
        check(False, "review: an unknown revision is refused")
    except RpcError as err:
        data = err.error.get("data") or {}
        check(err.code == 1007 and data.get("reason") == "unknown revision",
              "review: an unknown revision is refused", err.error)
    check(c.call("review/clear", {}) == {}, "review: review/clear")
    st = c.call("review/state", {})
    check(not st["active"] and not st["inline"], "review: cleared", st)


# --- step 2d: annotations and reveal --------------------------------------------


def verify_annotations_reveal(args, check):
    sb = Sandbox()
    hx = sb.start(args.hx)
    try:
        _verify_annotations_reveal(sb, hx, check)
    finally:
        hx.kill()
        if not args.keep:
            sb.cleanup()


def _verify_annotations_reveal(sb, hx, check):
    main_go = sb.path("main.go")
    reg = sb.registry(hx.pid)
    if not check(reg is not None, "2d: registry file written"):
        return
    c = Client(reg["socket"])
    hello = c.call("bridge/hello", {"client": "live-2d"})
    check({"annotations", "reveal"} <= set(hello["capabilities"]), "2d: capabilities", hello["capabilities"])

    m = hx.mark()
    r = c.call("annotations/set", {"namespace": "review", "path": main_go, "items": [
        {"line": 6, "endLine": 8, "text": "LIVE-COMMENT wrap this error"},
        {"line": 6, "endLine": 8, "kind": "reply", "text": "LIVE-REPLY done"},
    ]})
    check(r["count"] == 2, "2d: annotations/set count", r)
    check(hx.wait_text("LIVE-COMMENT wrap this error", m) and hx.wait_text("LIVE-REPLY done", m), "2d: annotation text on screen")

    # never part of the document: not in its text, its selection or the saved file
    t = c.call("editor/text", {"path": main_go})
    check("LIVE-COMMENT" not in t["text"], "2d: not in editor/text")
    hx.send("%")
    sel = c.call("editor/selection")
    check("LIVE-COMMENT" not in sel["ranges"][0]["text"], "2d: not in a whole-buffer selection")
    hx.send(";ggOabove" + ESC, pause=0.15)
    hx.send(":w" + ENTER, pause=0.3)
    with open(main_go) as f:
        check("LIVE-COMMENT" not in f.read(), "2d: not saved")
    c.call("editor/open", {"path": main_go, "line": 9, "interrupt": "always"})  # keys were just pressed
    sel = c.call("editor/selection")
    with open(main_go) as f:
        ninth = f.read().split("\n")[8]
    check(sel["ranges"][0]["text"] == ninth, "2d: line numbers unaffected by virtual lines", (sel["ranges"], ninth))

    # Helix redraws only changed cells: clear the status line with characters
    # the expected message does not share
    c.call("editor/status", {"message": "~" * 40})
    m = hx.mark()
    hx.send(":bb-annotations" + ENTER)
    # ("bb annotations" shares cells with the typed command line)
    check(hx.wait_text("hidden", m), "2d: :bb-annotations hides")
    m = hx.mark()
    hx.send(":bb-annotations" + ENTER)
    check(hx.wait_text("LIVE-COMMENT", m), "2d: :bb-annotations shows them again")

    # 500 annotations over the big file: requests and moves stay fast
    items = [{"line": 3 + 25 * i, "text": f"note {i}: something to say about this line"} for i in range(500)]
    t0 = time.perf_counter()
    c.call("annotations/set", {"namespace": "bulk", "path": main_go, "items": items})
    set_ms = (time.perf_counter() - t0) * 1000
    c.call("events/subscribe", {"events": ["selection"]})
    c.drain(0.2)
    lat = []
    for i in range(30):
        t = time.perf_counter()
        hx.send("\x04" if i % 2 == 0 else "\x15", pause=0)  # ctrl-d / ctrl-u: half-page scrolls
        c.event("event/selection")
        lat.append((time.perf_counter() - t) * 1000)
        time.sleep(0.08)
    lat.sort()
    print(f"INFO 2d: annotations/set of 500 items {set_ms:.1f}ms; key -> event/selection while scrolling over them: "
          f"p50={lat[len(lat)//2]:.2f}ms max={lat[-1]:.2f}ms")
    check(lat[len(lat) // 2] < 10, "2d: scrolling over 500 annotations stays fast (p50 < 10 ms)")
    c.call("annotations/clear", {"namespace": "bulk"})

    # reveal: removed line shows, any key ends it, keys are not delayed
    hx.send("gg")
    m = hx.mark()
    r = c.call("editor/reveal", {
        "path": main_go,
        "ranges": [{"start": pos(5, 1), "end": pos(12, 1)}],
        "removed": [{"line": 6, "text": "LIVE-REMOVED old line"}],
        "durationMs": 3000,
    })
    check(r == {"animated": 1, "deferred": False}, "2d: editor/reveal result", r)
    check(hx.wait_text("LIVE-REMOVED old line", m), "2d: removed line on screen")
    c.drain(0.2)
    lat = []
    for i in range(10):
        c.call("editor/reveal", {"path": main_go, "ranges": [{"start": pos(5, 1), "end": pos(20, 1)}], "durationMs": 3000})
        time.sleep(0.2)
        t = time.perf_counter()
        hx.send("k" if i % 2 else "j", pause=0)
        c.event("event/selection")
        lat.append((time.perf_counter() - t) * 1000)
        time.sleep(0.05)
    lat.sort()
    print(f"INFO 2d: key -> event/selection during a reveal: p50={lat[len(lat)//2]:.2f}ms max={lat[-1]:.2f}ms")
    check(lat[len(lat) // 2] < 10, "2d: a key during an animation is not delayed (p50 < 10 ms)")
    try:
        c.call("editor/reveal", {"path": main_go, "ranges": [], "durationMs": 5})
        check(False, "2d: durationMs out of range fails")
    except RpcError as e:
        check(e.code == -32602, "2d: durationMs out of range: invalid_params")
    hx.esc()
    hx.send(":qa!" + ENTER)
    check(hx.wait_exit(), "2d: hx exits")


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
    ap.add_argument("mode", choices=["verify", "files", "review", "bench", "compat", "lspbench", "lspbig", "redraw",
                                     "watch"])
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
    ap.add_argument("--before", help="redraw: the binary to compare against")
    ap.add_argument("--settle", type=float, default=0, help="lspbig: wait up to this long for gopls to go idle")
    args = ap.parse_args()
    modes = {"verify": verify, "files": verify_files_only, "review": verify_review_only, "bench": bench, "compat": compat,
             "lspbench": lambda a: live_lsp.bench(a, Sandbox), "lspbig": lambda a: live_lsp.big(a, Sandbox),
             "redraw": lambda a: redraw.bench(a, Sandbox), "watch": watch_only}
    sys.exit(modes[args.mode](args))


if __name__ == "__main__":
    main()
