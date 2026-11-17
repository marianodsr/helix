"""Helix watches the files of its open buffers (capability `watch`).
Used by `live.py verify` (checks) and `live.py watchbench` (numbers), always
on a real `hx` under a pty with a throwaway HOME.

Measures, from the moment a write to an open file has finished:
- until `event/reloaded` reaches a client, and
- until the new text is in hx's output to the terminal (the first frame of
  the reveal: the changed text is drawn, covered, from that frame on),
for writes in place and by rename-over; the CPU hx uses while 200 open
buffers do not change; the descriptors hx holds with 1,000 open buffers, at
the default limit and with a hard limit of 512.
"""

import os
import re
import shutil
import subprocess
import tempfile
import time

from bridge_client import Client, RpcError

ENTER = "\r"


def stats(xs):
    xs = sorted(xs)
    return {"n": len(xs), "p50": round(xs[len(xs) // 2], 2), "p90": round(xs[int(len(xs) * 0.9)], 2),
            "max": round(xs[-1], 2)}


def write(path, text, rename=False):
    if rename:
        tmp = path + ".tmp~"
        with open(tmp, "w") as f:
            f.write(text)
        os.rename(tmp, path)
    else:
        with open(path, "w") as f:
            f.write(text)


def until(f, timeout=5.0, step=0.002):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            r = f()
            if r:
                return r
        except RpcError:
            pass
        time.sleep(step)
    return None


def buffer_text(c, path):
    return c.call("editor/text", {"path": path})


def cpu_seconds(pid):
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    if not out:
        return None
    parts = [float(p) for p in out.replace("-", ":").split(":")]
    secs = 0.0
    for p in parts:
        secs = secs * 60 + p
    return secs


def open_fds(pid):
    out = subprocess.run(["lsof", "-n", "-P", "-p", str(pid)], capture_output=True, text=True).stdout
    return max(0, len(out.splitlines()) - 1)


def normal_file(n=300):
    lines = ["package main", "", 'import "fmt"', ""]
    for i in range(n // 5):
        lines += [f"func w{i}(x int) int {{", f'\tfmt.Println("w{i}", x)', f"\treturn x + {i}", "}", ""]
    return "\n".join(lines) + "\n"


def latency(sb, hx, c, path, rename, n=30):
    """Write a marker into line 3 of the open file `n` times."""
    base = open(path).read().split("\n")
    to_event, to_screen = [], []
    c.drain(0.2)
    for i in range(n):
        # every cell differs from the previous sample: Helix redraws only
        # the cells that changed, so the run arrives in one piece
        run = chr(ord("A") + (i + 13 * rename) % 26) * 16
        marker = "// " + run
        lines = list(base)
        lines[2] = marker
        m = hx.mark()
        write(path, "\n".join(lines), rename)
        t = time.perf_counter()
        seen = None
        got_event = None
        deadline = t + 3
        while time.perf_counter() < deadline and (seen is None or got_event is None):
            if seen is None and run in hx.text_since(m):
                seen = time.perf_counter()
            if got_event is None:
                ev = c.read(0.001)
                if ev is not None and ev.get("method") == "event/reloaded":
                    got_event = time.perf_counter()
        if got_event:
            to_event.append((got_event - t) * 1000)
        if seen:
            to_screen.append((seen - t) * 1000)
        time.sleep(0.25)
    return stats(to_event) if to_event else None, stats(to_screen) if to_screen else None, n


def _start(sb, hx_path, files, extra_env=None, wrapper=None):
    from live import Hx  # noqa: E402
    env = sb.env(**(extra_env or {}))
    args = ["-v"] + list(files)
    if wrapper:
        return Hx("/bin/sh", ["-c", wrapper + '; exec "$0" "$@"', hx_path] + args, sb.repo, env)
    return Hx(hx_path, args, sb.repo, env)


def verify(args, Sandbox, check):
    _verify_screen_latency(args, Sandbox, check)
    sb = Sandbox()
    try:
        sb.write("w.go", normal_file())
        sb.write("other.txt", "other\n")
        hx = _start(sb, args.hx, ["w.go", "other.txt"])
        try:
            _verify(args, sb, hx, check)
        finally:
            hx.kill()
    finally:
        if not args.keep:
            sb.cleanup()
    _verify_many(args, Sandbox, check)
    _verify_git(args, Sandbox, check)
    _verify_volume(args, Sandbox, check)


def _verify(args, sb, hx, check):
    w_go, other = sb.path("w.go"), sb.path("other.txt")
    reg = sb.registry(hx.pid)
    if not check(reg is not None, "watch: registry file written"):
        return
    c = Client(reg["socket"])
    hello = c.call("bridge/hello", {"client": "live-watch"})
    check("watch" in hello["capabilities"], "watch: capability listed", hello["capabilities"])
    c.call("events/subscribe", {"events": ["documents", "externalChange"]})
    c.call("editor/open", {"path": w_go, "line": 1, "interrupt": "always"})
    time.sleep(0.5)

    # latency, a 300-line file in view, reveal on: until event/reloaded
    for rename in (False, True):
        ev, _, n = latency(sb, hx, c, w_go, rename)
        how = "rename-over" if rename else "in place"
        print(f"INFO watch latency (reveal on), {how}: write -> event/reloaded {ev}")
        check(ev is not None and ev["n"] == n and ev["p50"] < 50, f"watch: {how}: every write reloaded, event p50 < 50 ms", ev)

    # Helix's own save reloads nothing and announces nothing
    c.drain(0.3)
    hx.send(":o " + w_go + ENTER, pause=0.2)
    hx.send("ggiown save <esc>".replace("<esc>", "\x1b"), pause=0.1)
    hx.send(":w" + ENTER, pause=0.5)
    got = [m["method"] for m in c.drain(0.5)]
    check("event/documentSaved" in got and "event/reloaded" not in got and "event/externalChange" not in got,
          "watch: :w reloads nothing", got)

    # unsaved changes: left alone, announced once, warned on the status line
    hx.send("ggiunsaved <esc>".replace("<esc>", "\x1b"), pause=0.2)
    c.drain(0.2)
    m = hx.mark()
    write(w_go, "someone else\n")
    ev = until(lambda: next((e for e in c.drain(0.05) if e["method"] == "event/externalChange"), None), 3)
    check(ev is not None and ev["params"]["path"] == w_go, "watch: modified buffer: event/externalChange", ev)
    check(hx.wait_text("changed on disk", m, 3), "watch: modified buffer: status-line warning")
    t = buffer_text(c, w_go)
    check(t["modified"] and t["text"].startswith("unsaved own save "), "watch: modified buffer untouched", t["text"][:40])
    hx.send(":reload" + ENTER, pause=0.3)
    check(until(lambda: not buffer_text(c, w_go)["modified"], 3), "watch: :reload discards the unsaved work")

    # bursts: ten writes 20 ms apart, ten writes a second apart
    final = None
    t0 = time.perf_counter()
    rtts = []
    for i in range(10):
        final = f"burst {i}\n" * 40
        write(w_go, final, rename=i % 2 == 0)
        s = time.perf_counter()
        c.call("editor/state")
        rtts.append((time.perf_counter() - s) * 1000)
        time.sleep(0.02)
    ok = until(lambda: buffer_text(c, w_go)["text"] == final, 3)
    check(ok, "watch: ten writes 20 ms apart: the last one shown", f"{(time.perf_counter() - t0):.2f}s")
    check(max(rtts) < 50, "watch: editor responsive during the burst", f"max editor/state {max(rtts):.1f} ms")
    shown = 0
    for i in range(10):
        want = f"slow {i}\n" * 40
        write(w_go, want)
        if until(lambda: buffer_text(c, w_go)["text"] == want, 0.9):
            shown += 1
        time.sleep(max(0.0, 1.0 - 0.0))
    check(shown == 10, "watch: ten writes a second apart: each shown within 0.9 s", shown)

    # deleted, then created again; a renamed directory
    m = hx.mark()
    os.remove(other)
    check(hx.wait_text("deleted on disk", m, 3), "watch: deleted file: status-line note")
    check(buffer_text(c, other)["text"] == "other\n", "watch: deleted file: buffer kept")
    write(other, "back\n")
    check(until(lambda: buffer_text(c, other)["text"] == "back\n", 3), "watch: created again: reloaded")
    write(other, "and again\n")
    check(until(lambda: buffer_text(c, other)["text"] == "and again\n", 3), "watch: created again: still watched")

    d = os.path.join(sb.repo, "dir")
    os.makedirs(d)
    inner = os.path.join(d, "f.txt")
    write(inner, "inner\n")
    inner = os.path.realpath(inner)
    c.call("editor/open", {"path": inner, "interrupt": "always"})
    time.sleep(0.3)
    # a key clears the earlier "deleted" note: Helix redraws only the cells
    # that change, so a similar message would not be written out whole
    hx.send("\x1b", pause=0.2)
    m = hx.mark()
    os.rename(d, d + "2")
    check(hx.wait_text("deleted on disk", m, 3), "watch: directory renamed: the buffer's file is gone (note)",
          repr(hx.text_since(m)[-300:]))
    os.rename(d + "2", d)
    hx.send("\x1b[I", pause=0.3)  # focus in: the focus check watches it again
    write(inner, "inner changed\n")
    check(until(lambda: buffer_text(c, inner)["text"] == "inner changed\n", 3),
          "watch: directory back: watched again (after the focus check)")

    # a 20k-line file rewritten completely
    big = os.path.join(sb.repo, "big.go")
    lines = ["package main", ""] + [f"func g{i}() int {{ return {i} }}" for i in range(20000)]
    write(big, "\n".join(lines) + "\n")
    big = os.path.realpath(big)
    c.call("editor/open", {"path": big, "line": 10000, "interrupt": "always"})
    time.sleep(0.5)
    c.drain(0.2)
    rewritten = "\n".join(l.replace("return", "return 1 +") for l in lines) + "\n"
    write(big, rewritten)
    t = time.perf_counter()
    ev = until(lambda: next((e for e in c.drain(0.01) if e["method"] == "event/reloaded"), None), 5)
    took = (time.perf_counter() - t) * 1000
    print(f"INFO watch: 20k-line file rewritten completely: event/reloaded after {took:.0f} ms")
    check(ev is not None and buffer_text(c, big)["text"] == rewritten, "watch: 20k-line file rewritten: reloaded")
    hx.send(":qa!" + ENTER)
    hx.wait_exit()


def _verify_screen_latency(args, Sandbox, check):
    """With the reveal off the new text is drawn as soon as it is reloaded
    (with it on, a line still covered is drawn blank until the animation
    reaches it, by design)."""
    sb = Sandbox(extra_config="[editor.bridge]\nreveal = false\n")
    try:
        sb.write("w.go", normal_file())
        hx = _start(sb, args.hx, ["w.go"])
        try:
            reg = sb.registry(hx.pid)
            c = Client(reg["socket"])
            c.call("events/subscribe", {"events": ["documents"]})
            time.sleep(0.5)
            for rename in (False, True):
                ev, screen, n = latency(sb, hx, c, sb.path("w.go"), rename)
                how = "rename-over" if rename else "in place"
                print(f"INFO watch latency (reveal off), {how}: write -> event/reloaded {ev}, "
                      f"write -> new text in the terminal output {screen}")
                check(screen is not None and screen["n"] == n and screen["p50"] < 50,
                      f"watch: {how}: new text on screen p50 < 50 ms", screen)
            hx.send(":qa!" + ENTER)
            hx.wait_exit()
        finally:
            hx.kill()
    finally:
        if not args.keep:
            sb.cleanup()


def _verify_many(args, Sandbox, check):
    """200 idle buffers cost no CPU; 1,000 buffers fit the descriptors."""
    for count, wrapper, label in ((200, None, "200 buffers"), (1000, "ulimit -Sn 256", "1000 buffers, soft limit 256"),
                                  (1000, "ulimit -Sn 256; ulimit -Hn 512", "1000 buffers, hard limit 512")):
        sb = Sandbox()
        try:
            os.makedirs(os.path.join(sb.repo, "many"))
            files = [f"many/f{i}.txt" for i in range(count)]
            for i, f in enumerate(files):
                sb.write(f, f"file {i}\n")
            hx = _start(sb, args.hx, files, wrapper=wrapper)
            try:
                reg = sb.registry(hx.pid, timeout=20)
                if not check(reg is not None, f"watch: {label}: started"):
                    continue
                c = Client(reg["socket"])
                time.sleep(3.0)
                m = hx.mark()
                hx.send(":bb-status" + ENTER, pause=0.3)
                status = re.search(r"watching (\d+) files(?: \((\d+) over the descriptor limit\))?",
                                   hx.text_since(m))
                watching = int(status.group(1)) if status else None
                over = int(status.group(2)) if status and status.group(2) else 0
                fds = open_fds(hx.pid)
                print(f"INFO watch: {label}: watching {watching}, over the limit {over}, hx holds {fds} descriptors")
                if count == 200:
                    before = cpu_seconds(hx.pid)
                    time.sleep(10)
                    after = cpu_seconds(hx.pid)
                    print(f"INFO watch: 200 open buffers, nothing changing: hx used {after - before:.2f} s CPU in 10 s")
                    check(after - before <= 0.02, "watch: 200 idle buffers: no measurable CPU", f"{after - before:.2f}s")
                    check(watching == 200, "watch: 200 buffers watched", watching)
                elif "hard" in label:
                    check(watching is not None and 0 < watching < count and over == count - watching,
                          "watch: hard limit 512: watches what fits, says how many it does not", (watching, over))
                else:
                    check(watching == count, "watch: soft limit raised, 1000 buffers watched", watching)
                # a watched file still reloads, and Helix can still open and write files
                target = sb.path(files[-1] if "hard" not in label else files[0])
                write(target, "changed\n")
                if "hard" not in label or True:
                    reloaded = until(lambda: buffer_text(c, target)["text"] == "changed\n", 3)
                    check(reloaded, f"watch: {label}: a change is reloaded")
                new = os.path.join(sb.repo, "new.txt")
                hx.send(":o new.txt" + ENTER, pause=0.3)
                hx.send("inew file\x1b", pause=0.1)
                hx.send(":w" + ENTER, pause=0.5)
                check(until(lambda: os.path.exists(new), 3), f"watch: {label}: Helix still opens and writes files")
                hx.send(":qa!" + ENTER)
                check(hx.wait_exit(), f"watch: {label}: exits cleanly")
            finally:
                hx.kill()
        finally:
            if not args.keep:
                sb.cleanup()


def _verify_git(args, Sandbox, check):
    """A git checkout changes twenty open files at once."""
    git = shutil.which("git")
    if not git:
        print("SKIP watch: git checkout (no git)")
        return
    sb = Sandbox()
    try:
        repo = os.path.join(sb.root, "gitrepo")
        os.makedirs(repo)
        g = ["git", "-C", repo, "-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]
        subprocess.run(g + ["init", "-q", "-b", "main"], check=True)
        files = [f"f{i}.txt" for i in range(20)]
        for f in files:
            with open(os.path.join(repo, f), "w") as h:
                h.write(f"main {f}\n")
        subprocess.run(g + ["add", "."], check=True)
        subprocess.run(g + ["commit", "-qm", "main"], check=True)
        subprocess.run(g + ["checkout", "-qb", "other"], check=True)
        for f in files:
            with open(os.path.join(repo, f), "w") as h:
                h.write(f"other {f}\n")
        subprocess.run(g + ["commit", "-qam", "other"], check=True)
        from live import Hx  # noqa: E402
        hx = Hx(args.hx, ["-v"] + files, repo, sb.env())
        try:
            reg = sb.registry(hx.pid, timeout=10)
            if not check(reg is not None, "watch: git: started"):
                return
            c = Client(reg["socket"])
            time.sleep(0.5)
            t = time.perf_counter()
            subprocess.run(g + ["checkout", "-q", "main"], check=True)
            paths = [os.path.realpath(os.path.join(repo, f)) for f in files]
            ok = until(lambda: all(buffer_text(c, p)["text"] == f"main {f}\n" for p, f in zip(paths, files)), 5)
            print(f"INFO watch: git checkout of 20 open files: all reloaded after {(time.perf_counter() - t) * 1000:.0f} ms")
            check(ok, "watch: git checkout: twenty open files reloaded")
            hx.send(":qa!" + ENTER)
            hx.wait_exit()
        finally:
            hx.kill()
    finally:
        if not args.keep:
            sb.cleanup()


def _verify_volume(args, Sandbox, check):
    """A file on another volume (a disk image attached for the test)."""
    sb = Sandbox()
    img = os.path.join(sb.root, "vol.dmg")
    mnt = os.path.join(sb.root, "mnt")
    os.makedirs(mnt)
    attached = False
    try:
        r = subprocess.run(["hdiutil", "create", "-size", "8m", "-fs", "APFS", "-volname", "bbwatch", "-quiet", img],
                           capture_output=True, text=True)
        if r.returncode == 0:
            r = subprocess.run(["hdiutil", "attach", "-nobrowse", "-quiet", "-mountpoint", mnt, img],
                               capture_output=True, text=True)
            attached = r.returncode == 0
        if not attached:
            print(f"SKIP watch: file on another volume (hdiutil: {r.stderr.strip()[:200]})")
            return
        path = os.path.join(mnt, "v.txt")
        write(path, "volume\n")
        hx = _start(sb, args.hx, [path])
        try:
            reg = sb.registry(hx.pid, timeout=10)
            c = Client(reg["socket"])
            time.sleep(0.5)
            real = os.path.realpath(path)
            write(path, "volume changed\n", rename=True)
            check(until(lambda: buffer_text(c, real)["text"] == "volume changed\n", 3),
                  "watch: a file on another volume (APFS disk image) is reloaded")
            hx.send(":qa!" + ENTER)
            hx.wait_exit()
        finally:
            hx.kill()
    finally:
        if attached:
            subprocess.run(["hdiutil", "detach", "-quiet", mnt], capture_output=True)
        if not args.keep:
            sb.cleanup()
