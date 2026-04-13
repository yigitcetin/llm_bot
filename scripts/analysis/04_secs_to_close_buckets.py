#!/usr/bin/env python3
"""Analysis 4: secs_to_close buckets — WR% and PnL global and per-asset."""

import json
from collections import defaultdict
from pathlib import Path

TRADES = Path(__file__).resolve().parents[2] / "data" / "trades.jsonl"

BUCKETS = [
    ("<600", lambda s: s < 600),
    ("600-750", lambda s: 600 <= s < 750),
    ("750-850", lambda s: 750 <= s < 850),
    ("850-950", lambda s: 850 <= s < 950),
    (">950", lambda s: s >= 950),
]


def won(r):
    o = r.get("outcome")
    if o is None:
        return None
    d = (r.get("direction") or "").upper()
    if d == "YES":
        return bool(o)
    if d == "NO":
        return not bool(o)
    return None


def bucket_for(secs):
    for name, fn in BUCKETS:
        if fn(secs):
            return name
    return "other"


def summarize(rows):
    wins = sum(1 for w, _ in rows if w)
    n = len(rows)
    wr = 100.0 * wins / n if n else 0.0
    pnl = sum(p for _, p in rows)
    return n, wr, pnl


def main():
    # (asset|None for global) -> bucket -> list of (won, pnl)
    data = defaultdict(lambda: defaultdict(list))
    with TRADES.open() as f:
        for line in f:
            r = json.loads(line)
            s = r.get("secs_to_close")
            if s is None:
                continue
            secs = int(s)
            w = won(r)
            if w is None:
                continue
            pnl = float(r.get("pnl") or 0)
            a = (r.get("asset") or "?").lower()
            b = bucket_for(secs)
            data[None][b].append((w, pnl))
            data[a][b].append((w, pnl))

    print("=== 4. secs_to_close buckets (trades.jsonl) ===\n")
    order = [b[0] for b in BUCKETS]

    print("GLOBAL:")
    for b in order:
        n, wr, pnl = summarize(data[None][b])
        print(f"  {b:10s}  n={n:3d}  WR%={wr:7.2f}  sum_pnl={pnl:12.4f}")

    assets = sorted(k for k in data.keys() if k is not None)
    for a in assets:
        print(f"\n{a.upper()}:")
        for b in order:
            n, wr, pnl = summarize(data[a][b])
            if n == 0:
                continue
            print(f"  {b:10s}  n={n:3d}  WR%={wr:7.2f}  sum_pnl={pnl:12.4f}")


if __name__ == "__main__":
    main()
