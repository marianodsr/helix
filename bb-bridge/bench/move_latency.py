#!/usr/bin/env python3
"""Key-to-output latency of cursor moves in a real `hx` under a pty.

    move_latency.py [--hx PATH] [--runtime DIR] [--file F] [--cols 53,100,140]
                    [--rows 40] [-n 100] [--key j] [--check MS]
                    [--diff-base BASE [--inline-diff]]

Starts the given `hx` under a pty at each width with a throwaway HOME, XDG
directories and a config that only turns language servers off (no bridge;
PATH is /usr/bin:/bin), opens the file (`--goto N`: then goes to line N), waits until the screen settles, then sends
one key at a time and measures from the write to the last byte of output it
causes (output followed by `--quiet` ms of silence). Prints p50 / p95 / max
per width. With `--check MS`, exits 1 if any width's p50 is above MS: that
is the regression check for wide Go buffers (`git bisect run` friendly:
exit 125 when hx does not start).

The default file is net/http/server.go from the Go source (4,100 lines).
`--runtime` defaults to the runtime/ next to the binary's checkout (for
target/release/hx), else $HELIX_RUNTIME. `--grammars DIR` links prebuilt
grammars into a copy of the runtime (for bisecting old commits whose
runtime/ has no built grammars). Mind that hx before c250852cc (Dec 2025)
loads grammars as `.so` on macOS and later ones as `.dylib`: with the wrong
names Go is silently not highlighted, and fast. `screen_dump.py` shows
whether it is.
"""

import argparse
import fcntl
import glob
import os
import pty
import select
import shutil
import signal
import statistics
import struct
import sys
import subprocess
import tempfile
import termios
import time

DEFAULT_FILES = [
    "/opt/homebrew/opt/go/libexec/src/net/http/server.go",
    "/usr/local/go/src/net/http/server.go",
]


def find_runtime(hx):
    real = os.path.realpath(hx)
    d = os.path.dirname(real)
    for _ in range(4):
        for cand in (os.path.join(d, "runtime"), os.path.join(d, "libexec", "runtime")):
            if os.path.isdir(os.path.join(cand, "queries")):
                return cand
        d = os.path.dirname(d)
    return os.environ.get("HELIX_RUNTIME")


def runtime_with_grammars(runtime, grammars, root):
    """A runtime whose queries/themes are `runtime`'s and grammars `grammars`'s."""
    rt = os.path.join(root, "runtime")
    os.makedirs(rt)
    for entry in os.listdir(runtime):
        if entry != "grammars":
            os.symlink(os.path.join(runtime, entry), os.path.join(rt, entry))
    os.symlink(grammars, os.path.join(rt, "grammars"))
    return rt


def drain(fd, quiet, limit):
    """Read until `quiet` seconds without output (or `limit`); returns
    (bytes read, time of first byte, time of last byte). Answers the
    terminal's primary device attributes query, as a terminal would, so hx
    does not wait for its query timeout."""
    n, first, last = 0, None, None
    deadline = time.perf_counter() + limit
    while True:
        now = time.perf_counter()
        wait = quiet if last is None else last + quiet - now
        if first is None:
            wait = deadline - now
        if wait <= 0 or now > deadline:
            return n, first, last
        r, _, _ = select.select([fd], [], [], wait)
        if not r:
            if first is not None or now + wait >= deadline:
                return n, first, last
            continue
        try:
            data = os.read(fd, 1 << 16)
        except OSError:
            return n, first, last
        if not data:
            return n, first, last
        t = time.perf_counter()
        if b"\x1b[c" in data:
            os.write(fd, b"\x1b[?62;22c")
        if first is None:
            first = t
        last = t
        n += len(data)


def measure(hx, runtime, path, cols, rows, n, key, quiet, root, goto=0, extra=""):
    home = tempfile.mkdtemp(prefix="home", dir=root)
    for d in ("home", "cache", "data", "state", "config/helix"):
        os.makedirs(os.path.join(home, d), exist_ok=True)
    with open(os.path.join(home, "config/helix/config.toml"), "w") as f:
        f.write("[editor.lsp]\nenable = false\n" + extra)
    env = {
        "HOME": os.path.join(home, "home"),
        "XDG_CONFIG_HOME": os.path.join(home, "config"),
        "XDG_CACHE_HOME": os.path.join(home, "cache"),
        "XDG_DATA_HOME": os.path.join(home, "data"),
        "XDG_STATE_HOME": os.path.join(home, "state"),
        "HELIX_RUNTIME": runtime,
        "HELIX_DISABLE_AUTO_GRAMMAR_BUILD": "1",
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "PATH": "/usr/bin:/bin",
        "LANG": "en_US.UTF-8",
    }
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(os.path.dirname(path))
        os.execve(hx, [hx, path], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    try:
        # startup and the first highlighted paint
        got, _, _ = drain(fd, 0.5, 15.0)
        if not got:
            return None
        if goto:
            os.write(fd, f"{goto}G".encode())
            drain(fd, 0.3, 3.0)
        for _ in range(3):  # warm-up moves, not counted
            os.write(fd, key.encode())
            drain(fd, quiet, 3.0)
        lat = []
        for _ in range(n):
            t0 = time.perf_counter()
            os.write(fd, key.encode())
            got, first, last = drain(fd, quiet, 2.0)
            if not got:
                continue
            lat.append(((first - t0) * 1000, (last - t0) * 1000))
        return lat
    finally:
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass
        os.close(fd)


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    here = os.path.dirname(os.path.abspath(__file__))
    ap.add_argument("--hx", default=os.path.join(here, "..", "..", "target", "release", "hx"))
    ap.add_argument("--runtime")
    ap.add_argument("--grammars")
    ap.add_argument("--file")
    ap.add_argument("--cols", default="53,100,140")
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("-n", type=int, default=100)
    ap.add_argument("--key", default="j")
    ap.add_argument("--goto", type=int, default=0, help="go to this line first")
    ap.add_argument("--quiet", type=float, default=30.0, help="ms of silence that ends a key's output")
    ap.add_argument("--check", type=float, help="exit 1 if a width's p50 is above this (ms)")
    ap.add_argument("--label", default="")
    ap.add_argument("--diff-base", help="commit this file as the file's base in a throwaway git repository, "
                    "so Helix diffs against it (step 5)")
    ap.add_argument("--inline-diff", action="store_true", help="[editor.bridge] inline-diff = true (step 5)")
    a = ap.parse_args()

    hx = os.path.abspath(a.hx)
    if not os.access(hx, os.X_OK):
        print(f"no hx at {hx}", file=sys.stderr)
        sys.exit(125)
    path = a.file or next((f for f in DEFAULT_FILES if os.path.exists(f)), None)
    if not path:
        sys.exit("no --file and no net/http/server.go found")
    base = os.environ.get("TMPDIR", "/tmp")
    root = tempfile.mkdtemp(prefix="mvlat", dir=base if len(base) < 60 else "/tmp")
    try:
        runtime = a.runtime or find_runtime(hx)
        if not runtime:
            sys.exit("no runtime: pass --runtime")
        if a.grammars:
            runtime = runtime_with_grammars(os.path.abspath(runtime), os.path.abspath(a.grammars), root)
        # a private copy of the file, so nothing can write to the original
        work = os.path.join(root, "work")
        os.makedirs(work)
        copy = os.path.join(work, os.path.basename(path))
        if a.diff_base:
            git = ["git", "-C", work, "-c", "user.name=b", "-c", "user.email=b@example.com",
                   "-c", "commit.gpgsign=false"]
            subprocess.run(git + ["init", "-q"], check=True)
            shutil.copy(a.diff_base, copy)
            subprocess.run(git + ["add", "-A"], check=True)
            subprocess.run(git + ["commit", "-qm", "base"], check=True)
        shutil.copy(path, copy)
        lines = sum(1 for _ in open(copy, "rb"))
        print(f"{a.label}hx={hx} file={os.path.basename(path)} ({lines} lines) rows={a.rows} key={a.key!r} n={a.n}")
        worst = 0.0
        for cols in [int(c) for c in a.cols.split(",")]:
            extra = "[editor.bridge]\ninline-diff = true\n" if a.inline_diff else ""
            lat = measure(hx, runtime, copy, cols, a.rows, a.n, a.key, a.quiet / 1000, root, a.goto, extra)
            if not lat:
                print(f"  {cols:>4} cols: hx produced no output", file=sys.stderr)
                sys.exit(125)
            firsts = [f for f, _ in lat]
            lasts = [l for _, l in lat]
            p50 = statistics.median(lasts)
            worst = max(worst, p50)
            print(
                f"  {cols:>4} cols: key to end of output p50 {p50:6.2f}  p95 {pct(lasts, 95):6.2f}  max {max(lasts):6.2f} ms"
                f"   (to first byte p50 {statistics.median(firsts):6.2f})  [{len(lat)} moves]"
            )
            sys.stdout.flush()
        if a.check is not None and worst > a.check:
            sys.exit(1)
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
