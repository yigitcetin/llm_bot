#!/usr/bin/env python3
"""Analysis 3: TIE trades stats + profitable TIE shadows and tie multiplier headroom."""

import json
import statistics
from collections import Counter, defaultdict
from pathlib import Path

TRADES = Path(__file__).resolve().parents[2] / "data" / "trades.jsonl"
SHADOW = Path(__file__).resolve().parents[2] / "data" / "shadow_trades.jsonl"


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


def pct(vals, p):
    if not vals:
        return float("nan")
    s = sorted(vals)
    k = (len(s) - 1) * p / 100.0
    f = int(k)
    c = min(f + 1, len(s) - 1)
    return s[f] + (k - f) * (s[c] - s[f])


def main():
    per = defaultdict(list)
    with TRADES.open() as f:
        for line in f:
            r = json.loads(line)
            if (r.get("cluster_direction") or "") != "TIE":
                continue
            a = (r.get("asset") or "?").lower()
            w = won(r)
            if w is None:
                continue
            edge = float(r.get("edge") or 0)
            pnl = float(r.get("pnl") or 0)
            per[a].append((w, pnl, edge))

    print("=== 3a. trades.jsonl — cluster_direction=TIE (per asset) ===\n")
    print(f"{'asset':8s} {'n':>4s} {'WR%':>8s} {'sum_pnl':>12s} {'avg_edge':>10s}")
    all_rows = []
    for a in sorted(per.keys()):
        xs = per[a]
        wins = sum(1 for w, _, _ in xs if w)
        n = len(xs)
        wr = 100.0 * wins / n if n else 0.0
        sp = sum(p for _, p, _ in xs)
        ae = sum(e for _, _, e in xs) / n if n else 0.0
        all_rows.extend(xs)
        print(f"{a:8s} {n:4d} {wr:8.2f} {sp:12.4f} {ae:10.6f}")
    if all_rows:
        wins = sum(1 for w, _, _ in all_rows if w)
        n = len(all_rows)
        wr = 100.0 * wins / n
        sp = sum(p for _, p, _ in all_rows)
        ae = sum(e for _, _, e in all_rows) / n
        print(f"{'GLOBAL':8s} {n:4d} {wr:8.2f} {sp:12.4f} {ae:10.6f}")

    ratios = []
    reasons = []
    with SHADOW.open() as f:
        for line in f:
            r = json.loads(line)
            if (r.get("cluster_direction") or "") != "TIE":
                continue
            pnl = float(r.get("pnl") or 0)
            if pnl <= 0:
                continue
            edge_s = r.get("edge")
            base_s = r.get("adaptive_min_edge")
            eff_s = r.get("effective_min_edge")
            if edge_s is None or base_s is None:
                continue
            edge = float(edge_s)
            base = float(base_s)
            eff = float(eff_s) if eff_s is not None else float("nan")
            m_vs_base = edge / base if base > 0 else float("nan")
            m_vs_eff = edge / eff if eff == eff and eff > 0 else float("nan")
            ratios.append((m_vs_base, m_vs_eff))
            reasons.append(r.get("skip_reason"))

    print("\n=== 3b. shadow_trades.jsonl — TIE and pnl>0 (profitable counterfactual shadows) ===\n")
    print(f"Count: {len(ratios)}")
    if ratios:
        mb = [x[0] for x in ratios if x[0] == x[0]]
        me = [x[1] for x in ratios if x[1] == x[1]]
        print(
            "edge / adaptive_min_edge (interpret as max cluster_tie-equivalent multiplier vs base adaptive edge):"
        )
        print(
            f"  min={min(mb):.4f}  p25={pct(mb,25):.4f}  med={statistics.median(mb):.4f}  "
            f"p75={pct(mb,75):.4f}  max={max(mb):.4f}"
        )
        print("\nedge / effective_min_edge (vs recorded effective min; >1 means edge cleared bar):")
        print(
            f"  min={min(me):.4f}  p25={pct(me,25):.4f}  med={statistics.median(me):.4f}  "
            f"p75={pct(me,75):.4f}  max={max(me):.4f}"
        )
        c = Counter(reasons)
        print("\nSkip reasons among these profitable TIE shadows:")
        for k, v in c.most_common():
            print(f"  {k}: {v}")




    # edge_too_small + profitable TIE only
    r2 = []
    with SHADOW.open() as f:
        for line in f:
            r = json.loads(line)
            if (r.get("cluster_direction") or "") != "TIE":
                continue
            if r.get("skip_reason") != "edge_too_small":
                continue
            pnl = float(r.get("pnl") or 0)
            if pnl <= 0:
                continue
            edge_s, base_s, eff_s = r.get("edge"), r.get("adaptive_min_edge"), r.get("effective_min_edge")
            if edge_s is None or base_s is None:
                continue
            edge, base = float(edge_s), float(base_s)
            eff = float(eff_s) if eff_s is not None else float("nan")
            r2.append((edge / base if base > 0 else float("nan"), edge / eff if eff == eff and eff > 0 else float("nan")))
    print("\n=== 3c. subset: TIE, pnl>0, skip_reason=edge_too_small ===\n")
    print(f"Count: {len(r2)}")
    if r2:
        mb = [a for a, _ in r2 if a == a]
        me = [b for _, b in r2 if b == b]
        print(f"edge/adaptive_min_edge: min={min(mb):.4f} p25={pct(mb,25):.4f} med={statistics.median(mb):.4f} p75={pct(mb,75):.4f} max={max(mb):.4f}")
        print(f"edge/effective_min_edge: min={min(me):.4f} p25={pct(me,25):.4f} med={statistics.median(me):.4f} p75={pct(me,75):.4f} max={max(me):.4f}")

if __name__ == "__main__":
    main()
