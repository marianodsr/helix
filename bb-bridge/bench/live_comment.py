"""`event/commentRequest` and `:bb-emit` against a real `hx`, used by
`live.py verify` and `live.py comment`.

The screen cells `event/commentRequest` reports are checked against the
screen itself: hx runs in a private tmux server (look.py's Scene: this
repository's config, so line numbers, the bufferline and soft wrap are on),
a known token is selected, `:bb-comment` is typed, and the cells must be
where tmux shows the token. With a plain view, virtual lines above (an
annotation), a vertical split, a soft-wrapped row and three whole lines.
"""

import argparse
import os
import re
import shutil
import time

import look

ANSI = re.compile(r"\x1b\[[0-9;:?]*[A-Za-z]")


def screen(s):
    return [ANSI.sub("", line) for line in s.tmux.capture().split("\n")]


def find(rows, needle, last=False):
    """(row, col) of `needle` on the screen (the last occurrence on its row
    with `last`, for the right pane of a split)."""
    for r, line in enumerate(rows):
        if needle in line:
            return r, (line.rindex(needle) if last else line.index(needle))
    return None


def line_of(text, needle):
    for i, line in enumerate(text.split("\n")):
        if needle in line:
            return i + 1, line.index(needle) + 1
    raise ValueError(needle)


def request(s, ranges):
    s.client.drain(0.1)
    s.call("editor/select", {"ranges": ranges, "center": False, "interrupt": "always"})
    time.sleep(0.25)
    s.tmux.keys(":bb-comment", "Enter")
    ev = s.client.event("event/commentRequest", timeout=3)
    time.sleep(0.2)
    return ev


def verify(args, check):
    if not shutil.which("tmux"):
        print("SKIP comment request: no tmux")
        return
    ns = argparse.Namespace(hx=args.hx, theme="catppuccin_macchiato", cols=120, rows=36, extra_config="",
                            out=None, scene=[], review=False, runtime=os.path.join(look.ROOT, "runtime"))
    s = look.Scene(ns)
    try:
        _verify(s, check)
    finally:
        s.close()


def _verify(s, check):
    c = s.client
    c.call("events/subscribe", {"events": ["commentPrompt", "send", "action"]})
    text = open(s.path).read()

    def token_case(label, needle, line_needle=None, last=False, before=None):
        ln, col = line_of(text, line_needle or needle)
        col += (line_needle or needle).index(needle) if line_needle else 0
        if before:
            before()
        ev = request(s, [{"start": {"line": ln, "col": col}, "end": {"line": ln, "col": col + len(needle)}}])
        rows = screen(s)
        where = find(rows, needle if not line_needle else line_needle, last)
        if where and line_needle:
            where = (where[0], where[1] + line_needle.index(needle))
        sc = ev.get("screen", {})
        want_top = {"row": where[0], "col": where[1]} if where else None
        want_bottom = {"row": where[0], "col": where[1] + len(needle) - 1} if where else None
        check(sc.get("top") == want_top and sc.get("bottom") == want_bottom,
              f"commentRequest: {label}: cells match the screen", {"got": sc, "want": [want_top, want_bottom]})
        check(ev.get("ranges", [{}])[0].get("text") == needle, f"commentRequest: {label}: the selected text", ev.get("ranges"))
        check("comment:" not in "\n".join(rows[-2:]), f"commentRequest: {label}: no Helix prompt opened")
        return ev

    s.goto(20)
    ev = token_case("plain view", "ErrNoListen", line_needle="var ErrNoListen")
    check(ev.get("screen", {}).get("rows") == 36 and ev["screen"].get("cols") == 120, "commentRequest: grid size", ev.get("screen"))
    check(ev.get("path") == s.path and "kind" not in ev and "note" not in ev, "commentRequest: fields", sorted(ev))

    # virtual lines above: an annotation of three rows two lines up
    ln, _ = line_of(text, "var ErrNoListen")
    s.annotate([{"line": ln - 2, "text": "one\ntwo\nthree"}], ns="live")
    time.sleep(0.3)
    token_case("under an annotation's virtual lines", "ErrNoListen", line_needle="var ErrNoListen")
    s.call("annotations/clear", {"namespace": "live"})

    # the right pane of a vertical split
    s.tmux.keys(":vsplit", "Enter")
    s.goto(20)
    token_case("right pane of a split", "ErrNoListen", line_needle="var ErrNoListen", last=True)
    s.tmux.keys("C-w", "o")

    # a soft-wrapped line: a word on its second row
    s.goto(20)
    token_case("second row of a soft-wrapped line", "Replicas", line_needle="Replicas or an empty")

    # three whole lines (x): first cell of the first, last character of the third
    ln, _ = line_of(text, "func Load(")
    s.goto(ln)
    ev = request(s, [{"start": {"line": ln, "col": 1}, "end": {"line": ln + 3, "col": 1}}])
    rows = screen(s)
    lines = text.split("\n")
    first = find(rows, lines[ln - 1].strip())
    third_text = lines[ln + 1].rstrip()
    third = find(rows, third_text.strip())
    want_top = {"row": first[0], "col": first[1]} if first else None
    want_bottom = {"row": third[0], "col": third[1] + len(third_text.strip()) - 1} if third else None
    check(ev.get("screen", {}).get("top") == want_top and ev["screen"].get("bottom") == want_bottom,
          "commentRequest: three whole lines: first cell, last character of the third line",
          {"got": ev.get("screen"), "want": [want_top, want_bottom]})

    # under the inline diff's removed lines (the file in git, a line removed on disk)
    repo = s.repo
    shutil.rmtree(os.path.join(repo, ".git"), ignore_errors=True)
    look.git(repo, "init", "-q")
    look.git(repo, "add", "-A")
    look.git(repo, "commit", "-qm", "base", "--no-gpg-sign")
    gone = "// ErrNoListen is returned when the listen address is empty.\n"
    assert gone in text
    text = text.replace(gone, "")
    with open(s.path, "w") as f:
        f.write(text)
    time.sleep(0.5)  # the watch reloads it
    s.tmux.keys(":bb-diff", "Enter")
    s.goto(20)
    time.sleep(0.4)
    check("ErrNoListen is returned" in "\n".join(screen(s)), "commentRequest: the removed line is drawn (inline diff)")
    token_case("under the inline diff's removed line", "ErrNoListen", line_needle="var ErrNoListen")
    s.tmux.keys(":bb-diff", "Enter")

    # :bb-emit
    c.drain(0.1)
    s.tmux.keys(":bb-emit review-next a b", "Enter")
    ev = c.event("event/action", timeout=3)
    check(ev.get("name") == "review-next" and ev.get("args") == ["a", "b"] and ev.get("mode") == "normal"
          and ev.get("path") == s.path, ":bb-emit: event/action", ev)
    s.tmux.keys("v", ":bb-emit pick", "Enter")
    ev = c.event("event/action", timeout=3)
    check(ev.get("name") == "pick" and ev.get("args") == [] and ev.get("mode") == "select", ":bb-emit from select mode", ev)
    s.tmux.keys("Escape")
    c.call("events/unsubscribe", {"events": ["action"]})
    s.tmux.keys(":bb-emit nothing-listens", "Enter")
    time.sleep(0.2)
    check("No bb client is listening for 'nothing-listens'" in "\n".join(screen(s)), ":bb-emit without a client: an error")

    # with no client subscribed to commentPrompt, Helix's own prompt opens
    c.call("events/unsubscribe", {"events": ["commentPrompt"]})
    s.tmux.keys(":bb-comment", "Enter")
    time.sleep(0.3)
    check("comment:" in "\n".join(screen(s)), "commentRequest: without a subscriber the comment: prompt opens")
    s.tmux.keys("Escape")
