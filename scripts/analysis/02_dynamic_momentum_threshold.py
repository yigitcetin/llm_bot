#!/usr/bin/env python3
"""Analysis 2: momentum_5m_too_weak — |momentum_5m| distribution and avg vol by asset."""

import json
import statistics
from collections import defaultdict
from pathlib import Path

DATA = Path(__file__).resolve().parents[2] / "data" / "shadow_trades.jsonl"


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
        return {}
    return dict(
        n=len(vals),
        min=min(vals),
        p25=pct(vals, 25),
        median=statistics.median(vals),
        p75=pct(vals, 75),
        max=max(vals),
        mean=sum(vals) / len(vals),
    )


def main():
    by_asset_abs = defaultdict(list)
    by_asset_vol = defaultdict(list)

    with DATA.open() as f:
        for line in f:
            r = json.loads(line)
            if r.get("skip_reason") != "momentum_5m_too_weak":
                continue
            m = r.get("momentum_5m")
            if m is None:
                continue
            a = r.get("asset", "?").lower()
            by_asset_abs[a].append(abs(float(m)))
            v = r.get("volatility_std_pct")
            if v is not None:
                by_asset_vol[a].append(float(v))

    print("=== 2. Dynamic momentum threshold (skip_reason=momentum_5m_too_weak) ===\n")
    for a in sorted(by_asset_abs.keys()):
        s = summarize(by_asset_abs[a])
        avg_vol = sum(by_asset_vol[a]) / len(by_asset_vol[a]) if by_asset_vol[a] else float("nan")
        print(f"Asset: {a}  (n={s['n']})")
        print(
            f"  |momentum_5m|: min={s['min']:.8f}  p25={s['p25']:.8f}  median={s['median']:.8f}  "
            f"p75={s['p75']:.8f}  max={s['max']:.8f}  mean={s['mean']:.8f}"
        )
        if not by_asset_vol[a]:
            print("  avg volatility_std_pct: N/A (null in JSON — momentum filter runs before vol snapshot in pipeline)")
        else:
            print(f"  avg volatility_std_pct (same rows): {avg_vol:.6f}")
        print()


if __name__ == "__main__":
    main()
