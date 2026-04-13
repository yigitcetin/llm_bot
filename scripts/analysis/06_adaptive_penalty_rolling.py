#!/usr/bin/env python3
"""Analysis 6: rolling last-N=10 direction WR for SOL/DOGE/BNB and suggested penalty series."""

import json
from collections import deque, defaultdict
from pathlib import Path

TRADES = Path(__file__).resolve().parents[2] / "data" / "trades.jsonl"
ASSETS = {"sol", "doge", "bnb"}
N = 10


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


def penalty_from_wr(wr):
    """Mirror adaptive.rs spirit: low WR -> stricter (higher penalty on confidence)."""
    if wr is None:
        return None
    if wr < 0.45:
        return 0.10
    if wr > 0.55:
        return 0.0
    return 0.05


def main():
    rows = []
    with TRADES.open() as f:
        for line in f:
            r = json.loads(line)
            a = (r.get("asset") or "").lower()
            if a not in ASSETS:
                continue
            w = won(r)
            if w is None:
                continue
            d = (r.get("direction") or "").upper()
            ts = r.get("timestamp") or ""
            rows.append((ts, a, d, w))

    rows.sort(key=lambda x: x[0])

    print("=== 6. Rolling N=10 direction win-rate and suggested confidence penalty ===\n")
    print(
        "Rule (illustrative, separate deque per direction): "
        "last 10 resolved YES trades -> yes_wr; last 10 NO -> no_wr. "
        "penalty: wr<0.45 -> 0.10; 0.45-0.55 -> 0.05; >0.55 -> 0.0\n"
    )

    for asset in sorted(ASSETS):
        yes_q = deque(maxlen=N)
        no_q = deque(maxlen=N)
        series = []
        for ts, a, d, w in rows:
            if a != asset:
                continue
            if d == "YES":
                yes_q.append(w)
            elif d == "NO":
                no_q.append(w)
            y_wr = sum(yes_q) / len(yes_q) if len(yes_q) else None
            n_wr = sum(no_q) / len(no_q) if len(no_q) else None
            y_pen = penalty_from_wr(y_wr)
            n_pen = penalty_from_wr(n_wr)
            series.append((ts, d, w, y_wr, n_wr, y_pen, n_pen))

        print(f"--- {asset.upper()} (resolved trades in file: {len([x for x in rows if x[1]==asset])}) ---")
        if not series:
            print("  (no rows)\n")
            continue
        print(f"{'timestamp':28s} {'dir':3s} {'win':>4s} {'yes_WR':>8s} {'no_WR':>8s} {'yes_pen':>8s} {'no_pen':>8s}")
        for row in series:
            ts, d, w, y_wr, n_wr, y_pen, n_pen = row
            print(
                f"{ts[:28]:28s} {d:3s} {str(w):>4s} "
                f"{y_wr if y_wr is not None else float('nan'):8.4f} "
                f"{n_wr if n_wr is not None else float('nan'):8.4f} "
                f"{y_pen if y_pen is not None else float('nan'):8.4f} "
                f"{n_pen if n_pen is not None else float('nan'):8.4f}"
            )
        # summary: unique penalty pairs
        pairs = {(y_pen, n_pen) for *_, y_pen, n_pen in series}
        pairs_list = sorted(pairs, key=lambda t: ((t[0] is None), t[0] if t[0] is not None else -1, (t[1] is None), t[1] if t[1] is not None else -1))
        print(f"\n  Distinct (yes_pen, no_pen) pairs seen: {pairs_list}\n")


if __name__ == "__main__":
    main()
