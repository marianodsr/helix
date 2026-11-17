#!/usr/bin/env python3
"""Round-trip time of one bridge method: p50 / p90 / p99 / max.

    rtt.py [--socket PATH] [--method editor/state] [--params '{}'] [-n 2000]

Without --socket, uses the newest instance in <cache>/bridge. Requests are
sent one at a time; each sample is send-to-response as seen by this client.
"""

import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bridge_client import Client, instances  # noqa: E402


def percentile(sorted_samples, p):
    if not sorted_samples:
        return float("nan")
    k = min(len(sorted_samples) - 1, max(0, round(p / 100 * (len(sorted_samples) - 1))))
    return sorted_samples[k]


def measure(client, method, params, n, warmup=50):
    for _ in range(warmup):
        client.call(method, params)
    samples = []
    for _ in range(n):
        t = time.perf_counter()
        client.call(method, params)
        samples.append((time.perf_counter() - t) * 1000)
    samples.sort()
    return {
        "method": method,
        "n": n,
        "p50": percentile(samples, 50),
        "p90": percentile(samples, 90),
        "p99": percentile(samples, 99),
        "max": samples[-1],
        "mean": sum(samples) / n,
    }


def fmt(r):
    return (
        f"{r['method']:<18} n={r['n']:<5} p50={r['p50']:.3f}ms p90={r['p90']:.3f}ms "
        f"p99={r['p99']:.3f}ms max={r['max']:.3f}ms mean={r['mean']:.3f}ms"
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--socket")
    ap.add_argument("--method", default="editor/state")
    ap.add_argument("--params", default="{}")
    ap.add_argument("-n", type=int, default=2000)
    ap.add_argument("--json", action="store_true", help="print the result as JSON")
    args = ap.parse_args()
    path = args.socket
    if not path:
        found = instances()
        if not found:
            sys.exit("no running Helix bridge found")
        path = found[0]["socket"]
    client = Client(path)
    client.call("bridge/hello", {"client": "rtt", "clientVersion": "1"})
    result = measure(client, args.method, json.loads(args.params), args.n)
    print(json.dumps(result) if args.json else fmt(result))


if __name__ == "__main__":
    main()
