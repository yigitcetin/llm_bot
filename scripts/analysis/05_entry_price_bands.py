#!/usr/bin/env python3
"""Analysis 5: entry_price bands — WR/PnL per asset; cheap-token shadows blocked by min size."""

import json
from collections import defaultdict
from pathlib import Path

TRADES = Path(__file__).resolve().parents[2] / "data" / "trades.jsonl"
SHADOW = Path(__file__).resolve().parents[2] / "data" / "shadow_trades.jsonl"

BANDS = [
    ("<0.20", lambda x: x < 0.20),
    ("0.20-0.35", lambda x: 0.20 <= x < 0.35),
    ("0.35-0.45", lambda x: 0.35 <= x < 0.45),
    ("0.45-0.55", lambda x: 0.45 <= x <= 0.55),
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


def band_for(ep):
    for name, fn in BANDS:
        if fn(ep):
            return name
    return "other"


def summarize(rows):
    wins = sum(1 for w, _ in rows if w)
    n = len(rows)
    wr = 100.0 * wins / n if n else 0.0
    pnl = sum(p for _, p in rows)
    return n, wr, pnl


def main():
    data = defaultdict(lambda: defaultdict(list))
    glob = defaultdict(list)
    with TRADES.open() as f:
        for line in f:
            r = json.loads(line)
            ep = r.get("entry_price")
            if ep is None:
                continue
            entry = float(ep)
            w = won(r)
            if w is None:
                continue
            pnl = float(r.get("pnl") or 0)
            a = (r.get("asset") or "?").lower()
            b = band_for(entry)
            data[a][b].append((w, pnl))
            glob[b].append((w, pnl))

    order = [b[0] for b in BANDS]
    print("=== 5a. entry_price bands (trades.jsonl) — GLOBAL ===\n")
    for b in order:
        n, wr, pnl = summarize(glob[b])
        if n == 0:
            continue
        print(f"  {b:12s}  n={n:3d}  WR%={wr:7.2f}  sum_pnl={pnl:12.4f}")
    print("\n=== 5a (cont.) per asset ===\n")
    for a in sorted(data.keys()):
        print(f"{a.upper()}:")
        for b in order:
            n, wr, pnl = summarize(data[a][b])
            if n == 0:
                continue
            print(f"  {b:12s}  n={n:3d}  WR%={wr:7.2f}  sum_pnl={pnl:12.4f}")
        print()

    print("=== 5b. shadow_trades — skip_reason=order_size_below_minimum, entry_price < 0.35, pnl>0 ===\n")
    hits = []
    with SHADOW.open() as f:
        for line in f:
            r = json.loads(line)
            if r.get("skip_reason") != "order_size_below_minimum":
                continue
            ep = r.get("entry_price")
            if ep is None:
                continue
            entry = float(ep)
            if entry >= 0.35:
                continue
            pnl = float(r.get("pnl") or 0)
            if pnl <= 0:
                continue
            hits.append((r.get("asset"), entry, pnl))

    print(f"Count profitable cheap-entry shadows blocked by min order size: {len(hits)}")
    by_a = defaultdict(list)
    for a, e, p in hits:
        by_a[a].append((e, p))
    for a in sorted(by_a.keys()):
        xs = by_a[a]
        print(f"  {a}: n={len(xs)}  entry_price min/med/max = ", end="")
        es = sorted(x[0] for x in xs)
        med = es[len(es) // 2]
        print(f"{min(es):.4f} / {med:.4f} / {max(es):.4f}  sum_pnl={sum(x[1] for x in xs):.4f}")


if __name__ == "__main__":
    main()
