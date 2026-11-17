#!/usr/bin/env python3
"""Capture what annotations and the reveal animation look like in a real
`hx`, run in a private tmux server with the owner's configuration copied into
a throwaway HOME. Writes `<name>.ansi`, `<name>.txt` and `<name>.png` per
scene into OUT.

    look.py OUT [--hx PATH] [--theme catppuccin_macchiato] [--scene NAME ...]

Never touches the real bridge directory, config or tmux server.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
from bridge_client import Client  # noqa: E402
import ansi_png  # noqa: E402

GO = '''package config

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// Config is what the service reads at start.
type Config struct {
	Listen   string
	DataDir  string
	Replicas int
	Tags     []string
}

// ErrNoListen is returned when the listen address is empty.
var ErrNoListen = errors.New("config: listen address is empty")

// Load reads the configuration file at path and fills in defaults for anything it does not set, so that callers never see a zero Replicas or an empty DataDir.
func Load(path string) (*Config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	cfg := &Config{Replicas: 1}
	for _, line := range strings.Split(string(data), "\\n") {
		key, value, ok := strings.Cut(line, "=")
		if !ok {
			continue
		}
		switch strings.TrimSpace(key) {
		case "listen":
			cfg.Listen = strings.TrimSpace(value)
		case "data":
			cfg.DataDir = filepath.Clean(strings.TrimSpace(value))
		case "tags":
			cfg.Tags = strings.Split(value, ",")
		}
	}
	if cfg.Listen == "" {
		return nil, ErrNoListen
	}
	return cfg, nil
}

// Validate reports the first problem with cfg.
func (cfg *Config) Validate() error {
	if cfg.Replicas < 1 {
		return fmt.Errorf("config: replicas must be at least 1, got %d", cfg.Replicas)
	}
	return nil
}
'''


class Tmux:
    def __init__(self, root, cols, rows):
        self.sock = os.path.join(root, "tmux.sock")
        self.cols, self.rows = cols, rows

    def run(self, *args, check=True):
        return subprocess.run(
            ["tmux", "-S", self.sock, "-f", "/dev/null", *args],
            capture_output=True,
            text=True,
            check=check,
        )

    def start(self, cmd):
        self.run("new-session", "-d", "-x", str(self.cols), "-y", str(self.rows), cmd)
        self.run("set", "-g", "default-terminal", "tmux-256color", check=False)
        self.run("set", "-as", "terminal-features", ",*:RGB", check=False)

    def keys(self, *keys):
        self.run("send-keys", *keys)
        time.sleep(0.15)

    def capture(self):
        return self.run("capture-pane", "-p", "-e", "-N").stdout

    def kill(self):
        self.run("kill-server", check=False)


def theme_colors(theme):
    """ui.background and ui.text of a theme, for the PNG margin."""
    known = {
        "catppuccin_macchiato": ("#cad3f5", "#24273a"),
        "catppuccin_latte": ("#4c4f69", "#eff1f5"),
    }
    return known.get(theme, ("#cad3f5", "#24273a"))


class Scene:
    def __init__(self, args):
        self.args = args
        base = os.environ.get("TMPDIR", "/tmp")
        if len(base) > 40:
            base = "/tmp"
        self.root = tempfile.mkdtemp(prefix="hxlook", dir=base)
        for d in ("home", "cache", "data", "state", "config/helix", "repo/.git"):
            os.makedirs(os.path.join(self.root, d), exist_ok=True)
        cfg_src = os.path.join(ROOT, "bb-bridge", "config", "helix")
        cfg = os.path.join(self.root, "config", "helix")
        with open(os.path.join(cfg_src, "config.toml")) as f:
            config = f.read()
        if args.theme:
            config = config.replace('theme = "catppuccin_macchiato"', f'theme = "{args.theme}"')
        config += args.extra_config
        with open(os.path.join(cfg, "config.toml"), "w") as f:
            f.write(config)
        shutil.copy(os.path.join(cfg_src, "languages.toml"), cfg)
        self.repo = os.path.join(self.root, "repo")
        with open(os.path.join(self.repo, "config.go"), "w") as f:
            f.write(GO)
        self.path = os.path.realpath(os.path.join(self.repo, "config.go"))
        self.tmux = Tmux(self.root, args.cols, args.rows)
        env = {
            "HOME": os.path.join(self.root, "home"),
            "XDG_CONFIG_HOME": os.path.join(self.root, "config"),
            "XDG_CACHE_HOME": os.path.join(self.root, "cache"),
            "XDG_DATA_HOME": os.path.join(self.root, "data"),
            "XDG_STATE_HOME": os.path.join(self.root, "state"),
            "HELIX_RUNTIME": os.path.join(ROOT, "runtime"),
            "TERM": "tmux-256color",
            "COLORTERM": "truecolor",
            "PATH": "/usr/bin:/bin",
            "LANG": "en_US.UTF-8",
        }
        envs = " ".join(f"{k}={v}" for k, v in env.items())
        self.tmux.start(f"cd {self.repo} && env -i {envs} {args.hx} config.go")
        bridge = os.path.join(self.root, "cache", "helix", "bridge")
        deadline = time.monotonic() + 10
        reg = None
        while time.monotonic() < deadline and reg is None:
            for name in os.listdir(bridge) if os.path.isdir(bridge) else []:
                if name.endswith(".json"):
                    with open(os.path.join(bridge, name)) as f:
                        reg = json.load(f)
            time.sleep(0.05)
        if reg is None:
            print(self.tmux.capture())
            self.tmux.kill()
            raise SystemExit("hx did not start its bridge")
        self.client = Client(reg["socket"])
        self.client.call("bridge/hello", {"client": "look"})
        time.sleep(0.3)

    def call(self, method, params):
        return self.client.call(method, params)

    def annotate(self, items, ns="review"):
        return self.call("annotations/set", {"namespace": ns, "path": self.path, "items": items})

    def goto(self, line):
        self.call("editor/open", {"path": self.path, "line": line})
        time.sleep(0.15)

    def snap(self, name, delay=0.25):
        time.sleep(delay)
        text = self.tmux.capture()
        out = self.args.out
        with open(os.path.join(out, name + ".ansi"), "w") as f:
            f.write(text)
        with open(os.path.join(out, name + ".txt"), "w") as f:
            f.write(ansi_png.re.sub(r"\x1b\[[0-9;:]*m", "", text))
        fg, bg = theme_colors(self.args.theme)
        rows = ansi_png.parse(text, self.args.cols)
        ansi_png.render(
            rows,
            self.args.cols,
            os.path.join(out, name + ".png"),
            ansi_png.hex_color(fg),
            ansi_png.hex_color(bg),
        )
        print("wrote", name)

    def close(self):
        try:
            self.tmux.keys("Escape", ":qa!", "Enter")
        finally:
            self.tmux.kill()
            shutil.rmtree(self.root, ignore_errors=True)


COMMENT = {
    "line": 24,
    "endLine": 27,
    "text": "Should this wrap the error with the path? The caller only logs it.",
}
REPLY = {
    "line": 24,
    "endLine": 27,
    "kind": "reply",
    "text": "Yes. I changed it to fmt.Errorf(\"config: read %s: %w\", path, err) so the log says which file.",
}
NOTE = {
    "line": 24,
    "endLine": 27,
    "kind": "note",
    "text": "os.ReadFile reads the whole file; configs here are a few hundred bytes.",
}
LONG = {
    "line": 28,
    "endLine": 28,
    "text": (
        "This silently accepts a config with no data directory. Load fills in "
        "Replicas but not DataDir, and the doc comment above says callers never "
        "see an empty DataDir. Either default it here (the service uses ./data "
        "everywhere else) or make Validate reject it, and add a test for the "
        "empty case so this does not regress."
    ),
    "highlight": True,
}


def scenes(s, which):
    def want(name):
        return not which or name in which

    if want("one"):
        s.annotate([COMMENT])
        s.goto(20)
        s.snap("01-one-comment")
    if want("reply"):
        s.annotate([COMMENT, REPLY])
        s.goto(20)
        s.snap("02-comment-reply")
    if want("stacked"):
        s.annotate([COMMENT, REPLY, NOTE])
        s.goto(20)
        s.snap("03-three-stacked")
    if want("long"):
        s.annotate([COMMENT, REPLY, LONG])
        s.goto(30)
        s.snap("04-long-wraps")
    if want("severity"):
        s.annotate(
            [
                {"line": 51, "endLine": 53, "text": "Replicas over 9 are not supported by the scheduler.", "severity": "warning", "sign": "!"},
                {"line": 55, "text": "End of file: annotation on the last line.", "kind": "note"},
            ]
        )
        s.goto(48)
        s.snap("05-severity-last-line")
    if want("split"):
        s.annotate([COMMENT, REPLY, LONG])
        s.goto(24)
        # left half, and a quarter on the right: two widths of one buffer
        s.tmux.keys(":vsplit", "Enter")
        s.tmux.keys(":hsplit", "Enter")
        s.tmux.keys(":vsplit", "Enter")
        s.goto(24)
        s.snap("06-narrow-split")
        s.tmux.keys("C-w", "o")
    if want("softwrap"):
        s.annotate(
            [{"line": 22, "text": "The doc comment is one long line; it soft-wraps and the note goes under its last row."}]
        )
        s.goto(20)
        s.snap("07-soft-wrapped-line")
    if want("reveal"):
        s.call("annotations/clear", {"namespace": "review"})
        s.goto(30)
        s.call(
            "editor/reveal",
            {
                "path": s.path,
                "ranges": [{"start": {"line": 29, "col": 1}, "end": {"line": 43, "col": 1}}],
                "removed": [{"line": 29, "text": "\tfor _, line := range strings.Fields(string(data)) {"}],
                "durationMs": 3000,
            },
        )
        s.snap("08-reveal-start", delay=0.15)
        s.snap("09-reveal-mid", delay=1.0)
        s.snap("10-reveal-late", delay=1.2)
        s.snap("11-reveal-done", delay=1.5)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--hx", default=os.path.join(ROOT, "target", "debug", "hx"))
    ap.add_argument("--theme", default="catppuccin_macchiato")
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--rows", type=int, default=36)
    ap.add_argument("--extra-config", default="")
    ap.add_argument("--scene", action="append", default=[])
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)
    s = Scene(args)
    try:
        scenes(s, set(args.scene))
    finally:
        s.close()


if __name__ == "__main__":
    sys.exit(main())
