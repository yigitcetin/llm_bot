#!/usr/bin/env python3
"""Analysis 1: volatility_filter shadows — vol distribution and counterfactual PnL at min thresholds."""

import json
import statistics
from collections import defaultdict
from pathlib import Path

DATA = Path(__file__).resolve().parents[2] / "data" / "shadow_trades.jsonl"
THRESHOLDS = [0.020, 0.018, 0.015, 0.012]


def pct(vals, p):
    if not vals:
        return float("nan")
    s = sorted(vals)
    k = (len(s) - 1) * p / 100.0
    f = int(k)
    c = min(f + 1, len(s) - 1)
    return s[f] + (k - f) * (s[c] - s[f])


def summarize(vals):
    if not vals:
        return dict(min=float("nan"), p25=float("nan"), median=float("nan"), p75=float("nan"), max=float("nan"))
    return dict(
        min=min(vals),
        p25=pct(vals, 25),
        median=statistics.median(vals),
        p75=pct(vals, 75),
        max=max(vals),
    )


def main():
    rows = []
    with DATA.open() as f:
        for line in f:
            r = json.loads(line)
            if r.get("skip_reason") != "volatility_filter":
                continue
            v = r.get("volatility_std_pct")
            if v is None:
                continue
            pnl = float(r.get("pnl") or 0)
            rows.append((r.get("asset", "?").lower(), float(v), pnl))

    by_asset = defaultdict(list)
    for a, v, _ in rows:
        by_asset[a].append(v)

    print("=== 1. Volatility filter recalibration (skip_reason=volatility_filter) ===\n")
    print(f"Total volatility_filter shadow rows: {len(rows)}\n")
    print("volatility_std_pct distribution by asset (min, p25, median, p75, max):")
    for a in sorted(by_asset.keys()):
        s = summarize(by_asset[a])
        print(
            f"  {a:6s}  n={len(by_asset[a]):4d}  "
            f"min={s['min']:.6f}  p25={s['p25']:.6f}  med={s['median']:.6f}  "
            f"p75={s['p75']:.6f}  max={s['max']:.6f}"
        )

    print("\nCounterfactual: rows would pass vol floor if volatility_std_pct >= threshold (same recorded pnl).")
    print("PnL sums at vol_min_std_pct thresholds (global, all volatility_filter rows):\n")
    for t in THRESHOLDS:
        sel = [p for _, v, p in rows if v >= t]
        print(f"  threshold {t:.3f}:  n_pass={len(sel):4d}  sum_pnl={sum(sel):.4f}")

    print("\nPer-asset PnL at thresholds (sum of pnl where vol >= T):\n")
    print(f"{'asset':8s} " + " ".join(f"T={t:.3f}" for t in THRESHOLDS))
    for a in sorted(by_asset.keys()):
        parts = []
        for t in THRESHOLDS:
            s = sum(p for aa, v, p in rows if aa == a and v >= t)
            parts.append(f"{s:10.2f}")
        print(f"{a:8s} " + " ".join(parts))


if __name__ == "__main__":
    main()
