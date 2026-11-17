#!/usr/bin/env python3
"""Print the screen `hx` renders for a file, as highlight scopes.

    screen_dump.py --hx BIN [--runtime RT] [--grammars G] --file F
                   [--lang go] [--goto N] [--cols 140] [--rows 60]

Starts `hx` under a pty with a throwaway HOME and a probe theme that gives
every scope named in the language's highlights.scm and locals.scm (and every
prefix of one) its own colour, renders the file (`--goto N`: line N at the
top of the view), and prints each text row with runs of one colour shown as
`[scope:text]`. Unstyled text is printed bare. Two dumps of the same file
diff to the cells whose highlight changed. Without a gutter, so rows are
the file's lines from the top of the view.
"""

import argparse
import fcntl
import os
import pty
import re
import select
import shutil
import signal
import struct
import sys
import tempfile
import termios
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from move_latency import find_runtime, runtime_with_grammars  # noqa: E402


def scopes(runtime, lang):
    names = set()
    for f in ("highlights.scm", "locals.scm"):
        p = os.path.join(runtime, "queries", lang, f)
        if os.path.exists(p):
            names |= set(re.findall(r"@([a-z][\w.]*)", open(p).read()))
    out = set()
    for n in names:
        if n.startswith("local"):
            continue
        parts = n.split(".")
        out |= {".".join(parts[: i + 1]) for i in range(len(parts))}
    return sorted(out)


def emulate(data, rows, cols):
    """Just enough of a terminal for what hx writes: cursor moves, SGR
    foreground, erase."""
    grid = [[(" ", None)] * cols for _ in range(rows)]
    r = c = 0
    fg = None
    s = data.decode("utf-8", "replace")
    i = 0
    while i < len(s):
        ch = s[i]
        if ch == "\x1b":
            m = re.match(r"\x1b\[([0-9;:?]*)([A-Za-z@])", s[i:])
            if m:
                i += m.end()
                args, cmd = m.group(1), m.group(2)
                if "?" in args or ":" in args:
                    continue
                nums = [int(x) if x else 0 for x in args.split(";")] if args else []
                if cmd == "H":
                    r = (nums[0] if nums else 1) - 1
                    c = (nums[1] if len(nums) > 1 else 1) - 1
                elif cmd == "m":
                    j = 0
                    if not nums:
                        fg = None
                    while j < len(nums):
                        if nums[j] in (0, 39):
                            fg = None
                        elif nums[j] in (38, 48) and nums[j + 1 : j + 2] == [2]:
                            if nums[j] == 38:
                                fg = "%02x%02x%02x" % tuple(nums[j + 2 : j + 5])
                            j += 4
                        elif nums[j] in (38, 48) and nums[j + 1 : j + 2] == [5]:
                            j += 2
                        j += 1
                elif cmd == "J" and nums[:1] == [2]:
                    grid = [[(" ", None)] * cols for _ in range(rows)]
                elif cmd == "K" and 0 <= r < rows:
                    for x in range(max(c, 0), cols):
                        grid[r][x] = (" ", None)
                elif cmd == "C":
                    c += nums[0] if nums else 1
                elif cmd == "G":
                    c = (nums[0] if nums else 1) - 1
                continue
            m = re.match(r"\x1b[\]P_].*?(\x07|\x1b\\)", s[i:], re.S)
            i += m.end() if m else 2
            continue
        if ch == "\r":
            c = 0
        elif ch == "\n":
            r += 1
        elif ch >= " ":
            if 0 <= r < rows and 0 <= c < cols:
                grid[r][c] = (ch, fg)
            c += 1
        i += 1
    return grid


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--hx", required=True)
    ap.add_argument("--runtime")
    ap.add_argument("--grammars")
    ap.add_argument("--file", required=True)
    ap.add_argument("--lang", default="go")
    ap.add_argument("--goto", type=int, default=0)
    ap.add_argument("--cols", type=int, default=140)
    ap.add_argument("--rows", type=int, default=60)
    a = ap.parse_args()

    hx = os.path.abspath(a.hx)
    runtime = os.path.abspath(a.runtime or find_runtime(hx))
    base = os.environ.get("TMPDIR", "/tmp")
    root = tempfile.mkdtemp(prefix="scr", dir=base if len(base) < 60 else "/tmp")
    try:
        rt = runtime_with_grammars(runtime, os.path.abspath(a.grammars), root) if a.grammars else runtime
        names = scopes(runtime, a.lang)
        colours = {
            n: "%02x%02x%02x" % (16 + (k * 37) % 220, 16 + (k * 71) % 220, 16 + (k * 13) % 220)
            for k, n in enumerate(names)
        }
        by_colour = {v: k for k, v in colours.items()}
        assert len(by_colour) == len(colours), "probe colours collide"
        for d in ("home", "cache", "data", "state", "config/helix/themes"):
            os.makedirs(os.path.join(root, d))
        with open(root + "/config/helix/themes/probe.toml", "w") as f:
            f.write('"ui.text" = "#ffffff"\n"ui.virtual" = "#fefefe"\n')
            f.write('"ui.selection" = { bg = "#202020" }\n"ui.selection.primary" = { bg = "#202020" }\n')
            for n, col in colours.items():
                f.write(f'"{n}" = "#{col}"\n')
        with open(root + "/config/helix/config.toml", "w") as f:
            f.write('theme = "probe"\n[editor]\ngutters = []\n[editor.lsp]\nenable = false\n')
        name = os.path.join(root, os.path.basename(a.file))
        shutil.copy(a.file, name)
        env = {
            "HOME": root + "/home",
            "XDG_CONFIG_HOME": root + "/config",
            "XDG_CACHE_HOME": root + "/cache",
            "XDG_DATA_HOME": root + "/data",
            "XDG_STATE_HOME": root + "/state",
            "HELIX_RUNTIME": rt,
            "HELIX_DISABLE_AUTO_GRAMMAR_BUILD": "1",
            "TERM": "xterm-256color",
            "COLORTERM": "truecolor",
            "PATH": "/usr/bin:/bin",
            "LANG": "en_US.UTF-8",
        }
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(root)
            os.execve(hx, [hx, name], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", a.rows, a.cols, 0, 0))
        out = bytearray()

        def pump(quiet, limit):
            end, last = time.time() + limit, time.time()
            while time.time() < end and time.time() - last < quiet:
                if select.select([fd], [], [], 0.05)[0]:
                    try:
                        chunk = os.read(fd, 1 << 16)
                    except OSError:
                        return
                    out.extend(chunk)
                    if b"\x1b[c" in chunk:
                        os.write(fd, b"\x1b[?62;22c")  # primary device attributes
                    last = time.time()

        pump(0.8, 10)
        # the first frame can come before the parse; move to get a highlighted one
        for keys in (b"l", b"h") + ((f"{a.goto}G".encode(), b"zt") if a.goto else ()):
            os.write(fd, keys)
            pump(0.3, 3)
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        for row in emulate(bytes(out), a.rows, a.cols)[: a.rows - 2]:
            segs = []
            for ch, fg in row:
                if segs and segs[-1][0] == fg:
                    segs[-1][1] += ch
                else:
                    segs.append([fg, ch])
            line = "".join(
                t if fg not in by_colour or not t.strip() else f"[{by_colour[fg]}:{t}]" for fg, t in segs
            )
            print(line.rstrip())
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
