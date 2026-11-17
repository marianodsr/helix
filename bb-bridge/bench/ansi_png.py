#!/usr/bin/env python3
"""Render terminal text with SGR colours (as `tmux capture-pane -p -e`
prints it) to a PNG, to look at what Helix drew.

    ansi_png.py screen.ansi screen.png [--cols 120] [--bg '#24273a']

Needs Pillow. Cells are drawn on a fixed grid; wide characters take two.
"""

import argparse
import re
import sys
import unicodedata

from PIL import Image, ImageDraw, ImageFont

FONTS = [
    ("/System/Library/Fonts/Menlo.ttc", 0),
    ("/System/Library/Fonts/Apple Symbols.ttf", 0),
    ("/System/Library/Fonts/Apple Braille.ttf", 0),
    ("/System/Library/Fonts/Supplemental/Arial Unicode.ttf", 0),
]
BOLD = ("/System/Library/Fonts/Menlo.ttc", 1)
ITALIC = ("/System/Library/Fonts/Menlo.ttc", 2)

BASIC = [
    (0, 0, 0), (205, 49, 49), (13, 188, 121), (229, 229, 16),
    (36, 114, 200), (188, 63, 188), (17, 168, 205), (229, 229, 229),
    (102, 102, 102), (241, 76, 76), (35, 209, 139), (245, 245, 67),
    (59, 142, 234), (214, 112, 214), (41, 184, 219), (255, 255, 255),
]


def xterm256(n):
    if n < 16:
        return BASIC[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        return (steps[n // 36], steps[(n // 6) % 6], steps[n % 6])
    v = 8 + (n - 232) * 10
    return (v, v, v)


def width(ch):
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in "WF" else 1


def parse(text, cols):
    """Rows of cells: (char, fg, bg, attrs)."""
    rows = []
    for line in text.split("\n"):
        cells = []
        fg = bg = None
        attrs = set()
        i = 0
        while i < len(line):
            m = re.match(r"\x1b\[([0-9;:]*)m", line[i:])
            if m:
                i += m.end()
                params = [p for p in re.split("[;:]", m.group(1))] or ["0"]
                j = 0
                while j < len(params):
                    p = int(params[j] or 0)
                    if p == 0:
                        fg = bg = None
                        attrs = set()
                    elif p in (1, 2, 3, 4, 7, 9):
                        attrs.add(p)
                    elif p in (22, 23, 24, 27, 29):
                        attrs.discard({22: 1, 23: 3, 24: 4, 27: 7, 29: 9}[p])
                        if p == 22:
                            attrs.discard(2)
                    elif p in (38, 48) and j + 1 < len(params):
                        if params[j + 1] == "2":
                            c = tuple(int(x or 0) for x in params[j + 2 : j + 5])
                            j += 4
                        else:
                            c = xterm256(int(params[j + 2]))
                            j += 2
                        if p == 38:
                            fg = c
                        else:
                            bg = c
                    elif p == 39:
                        fg = None
                    elif p == 49:
                        bg = None
                    elif 30 <= p <= 37:
                        fg = BASIC[p - 30]
                    elif 40 <= p <= 47:
                        bg = BASIC[p - 40]
                    elif 90 <= p <= 97:
                        fg = BASIC[p - 90 + 8]
                    elif 100 <= p <= 107:
                        bg = BASIC[p - 100 + 8]
                    j += 1
                continue
            m = re.match(r"\x1b[^m]*?[A-Za-z]", line[i:])
            if m and line[i] == "\x1b":
                i += m.end()
                continue
            ch = line[i]
            i += 1
            w = width(ch)
            if w == 0 and cells:
                c = cells[-1]
                cells[-1] = (c[0] + ch,) + c[1:]
                continue
            cells.append((ch, fg, bg, frozenset(attrs)))
            if w == 2:
                cells.append(("", fg, bg, frozenset(attrs)))
        rows.append(cells[:cols])
    while rows and not rows[-1]:
        rows.pop()
    return rows


def render(rows, cols, out, default_fg, default_bg, size=15):
    fonts = [ImageFont.truetype(path, size, index=idx) for path, idx in FONTS]
    bold = ImageFont.truetype(BOLD[0], size, index=BOLD[1])
    italic = ImageFont.truetype(ITALIC[0], size, index=ITALIC[1])
    cw = int(round(fonts[0].getlength("M")))
    ch = int(size * 1.35)
    img = Image.new("RGB", (cols * cw + 16, len(rows) * ch + 16), default_bg)
    d = ImageDraw.Draw(img)
    cache = {}

    def font_for(c, attrs):
        base = bold if 1 in attrs else italic if 3 in attrs else fonts[0]
        key = (c, base)
        if key in cache:
            return cache[key]
        for f in [base] + fonts:
            try:
                if f.getmask(c).getbbox() is not None:
                    cache[key] = f
                    return f
            except Exception:
                pass
        cache[key] = base
        return base

    for y, cells in enumerate(rows):
        for x, (c, fg, bg, attrs) in enumerate(cells):
            fg = fg or default_fg
            bg = bg or default_bg
            if 7 in attrs:
                fg, bg = bg, fg
            if 2 in attrs:
                fg = tuple((a + b) // 2 for a, b in zip(fg, bg))
            px, py = 8 + x * cw, 8 + y * ch
            d.rectangle([px, py, px + cw - 1, py + ch - 1], fill=bg)
            if not c or c == " ":
                continue
            o = ord(c[0])
            if 0x2580 <= o <= 0x259F or 0x2500 <= o <= 0x257F:
                draw_block(d, c[0], px, py, cw, ch, fg)
            elif o == 0x2800:
                pass
            else:
                f = font_for(c, attrs)
                d.text((px, py + (ch - size) // 2 - 1), c, font=f, fill=fg)
            if 4 in attrs:
                d.line([px, py + ch - 2, px + cw, py + ch - 2], fill=fg)
            if 9 in attrs:
                d.line([px, py + ch // 2, px + cw, py + ch // 2], fill=fg)
    img.save(out)


def draw_block(d, c, x, y, w, h, fg):
    """Box drawing and block elements, drawn so they join across cells."""
    o = ord(c)
    if 0x2581 <= o <= 0x2588:  # lower eighths
        n = o - 0x2580
        d.rectangle([x, y + h - h * n // 8, x + w - 1, y + h - 1], fill=fg)
    elif 0x2589 <= o <= 0x258F:  # left eighths
        n = 0x2590 - o
        d.rectangle([x, y, x + w * n // 8 - 1, y + h - 1], fill=fg)
    elif c == "▔":
        d.rectangle([x, y, x + w - 1, y + h // 8], fill=fg)
    elif c in "│┃":
        t = 1 if c == "│" else 3
        d.rectangle([x + w // 2 - t // 2, y, x + w // 2 + (t - 1) // 2, y + h - 1], fill=fg)
    elif c in "─━":
        t = 1 if c == "─" else 3
        d.rectangle([x, y + h // 2 - t // 2, x + w - 1, y + h // 2 + (t - 1) // 2], fill=fg)
    elif c in "└┘┌┐├┤┴┬┼╰╯╭╮":
        cx, cy = x + w // 2, y + h // 2
        up = c in "└┘├┤┴┼╰╯"
        down = c in "┌┐├┤┬┼╭╮"
        left = c in "┘┐┤┴┬┼╯╮"
        right = c in "└┌├┴┬┼╰╭"
        if up:
            d.line([cx, y, cx, cy], fill=fg)
        if down:
            d.line([cx, cy, cx, y + h - 1], fill=fg)
        if left:
            d.line([x, cy, cx, cy], fill=fg)
        if right:
            d.line([cx, cy, x + w - 1, cy], fill=fg)
    else:
        f = ImageFont.truetype(FONTS[0][0], 15)
        d.text((x, y), c, font=f, fill=fg)


def hex_color(s):
    s = s.lstrip("#")
    return tuple(int(s[i : i + 2], 16) for i in (0, 2, 4))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("src")
    ap.add_argument("out")
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--fg", default="#cad3f5")
    ap.add_argument("--bg", default="#24273a")
    args = ap.parse_args()
    with open(args.src, encoding="utf-8", errors="replace") as f:
        rows = parse(f.read(), args.cols)
    render(rows, args.cols, args.out, hex_color(args.fg), hex_color(args.bg))


if __name__ == "__main__":
    sys.exit(main())
