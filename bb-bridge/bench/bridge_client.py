"""Minimal client for the Helix bridge (bb-bridge/PROTOCOL.md).

Standard library only. Used by rtt.py and live.py; also handy from a REPL:

    from bridge_client import Client, instances
    c = Client(instances()[0]["socket"])
    c.call("editor/state")
"""

import collections
import json
import os
import select
import socket
import time


class RpcError(Exception):
    def __init__(self, error):
        super().__init__(f"{error.get('code')}: {error.get('message')}")
        self.error = error
        self.code = error.get("code")


def bridge_dir(cache_home=None):
    """<cache_dir>/bridge, as Helix computes it (XDG on macOS and Linux)."""
    cache_home = cache_home or os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache")
    return os.path.join(cache_home, "helix", "bridge")


def instances(directory=None):
    """Registry files of running instances, newest first."""
    directory = directory or bridge_dir()
    found = []
    try:
        names = os.listdir(directory)
    except FileNotFoundError:
        return []
    for name in names:
        if name.endswith(".json"):
            try:
                with open(os.path.join(directory, name)) as f:
                    found.append(json.load(f))
            except (OSError, ValueError):
                pass
    return sorted(found, key=lambda r: r.get("startedAt", ""), reverse=True)


class Client:
    def __init__(self, path, timeout=5.0):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.timeout = timeout
        self.buf = b""
        self.next_id = 0
        self.events = collections.deque()

    def close(self):
        self.sock.close()

    def send_raw(self, line):
        if isinstance(line, str):
            line = line.encode()
        self.sock.sendall(line + b"\n")

    def send(self, method, params=None):
        self.next_id += 1
        msg = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}}
        self.send_raw(json.dumps(msg))
        return self.next_id

    def notify(self, method, params=None):
        self.send_raw(json.dumps({"jsonrpc": "2.0", "method": method, "params": params or {}}))

    def read(self, timeout=None):
        """Next message, or None on timeout. Raises EOFError when closed."""
        deadline = time.monotonic() + (self.timeout if timeout is None else timeout)
        while b"\n" not in self.buf:
            left = deadline - time.monotonic()
            if left <= 0:
                return None
            ready, _, _ = select.select([self.sock], [], [], left)
            if not ready:
                return None
            chunk = self.sock.recv(1 << 16)
            if not chunk:
                raise EOFError("bridge closed the connection")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def response(self, id):
        while True:
            msg = self.read()
            if msg is None:
                raise TimeoutError(f"no answer to request {id}")
            if "method" in msg:
                self.events.append(msg)
            elif msg.get("id") == id:
                return msg

    def request(self, method, params=None):
        return self.response(self.send(method, params))

    def call(self, method, params=None):
        msg = self.request(method, params)
        if "error" in msg:
            raise RpcError(msg["error"])
        return msg["result"]

    def event(self, method, timeout=None):
        """Params of the next notification `method`; others are kept."""
        for i, msg in enumerate(self.events):
            if msg["method"] == method:
                del self.events[i]
                return msg["params"]
        deadline = time.monotonic() + (self.timeout if timeout is None else timeout)
        while True:
            msg = self.read(max(0.0, deadline - time.monotonic()))
            if msg is None:
                raise TimeoutError(f"no {method}")
            if msg.get("method") == method:
                return msg["params"]
            self.events.append(msg)

    def drain(self, seconds):
        out = list(self.events)
        self.events.clear()
        deadline = time.monotonic() + seconds
        while True:
            msg = self.read(max(0.0, deadline - time.monotonic()))
            if msg is None:
                return out
            out.append(msg)
