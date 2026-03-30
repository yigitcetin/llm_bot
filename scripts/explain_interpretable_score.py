#!/usr/bin/env python3
"""
analyze --json çıktısından interpretable skor bileşenlerini satır satır açıklar.

Kullanım:
  cargo run --bin analyze -- --json > /tmp/a.json
  ./scripts/explain_interpretable_score.py /tmp/a.json
  cat /tmp/a.json | ./scripts/explain_interpretable_score.py -

optimize_analyze.py ile aynı formül (import).
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import math
import sys
from pathlib import Path
from typing import Any


def _load_opt_module():
    root = Path(__file__).resolve().parent / "optimize_analyze.py"
    spec = importlib.util.spec_from_file_location("optimize_analyze", root)
    if spec is None or spec.loader is None:
        raise RuntimeError("optimize_analyze.py yüklenemedi")
    mod = importlib.util.module_from_spec(spec)
    # exec_module + @dataclass için modül sys.modules'ta olmalı
    sys.modules["optimize_analyze"] = mod
    spec.loader.exec_module(mod)
    return mod


def main() -> int:
    ap = argparse.ArgumentParser(description="Interpretable skor dökümü (analyze JSON)")
    ap.add_argument(
        "json_path",
        nargs="?",
        default="-",
        help="analyze JSON dosyası veya - (stdin)",
    )
    ap.add_argument("--horizon", type=int, default=30, help="Skorda kullanılan ufuk (gün)")
    ap.add_argument("--max-vol-skip-pct", type=float, default=92.0)
    ap.add_argument("--min-wf-oos-trades", type=int, default=5)
    ap.add_argument("--iw-sharpe", type=float, default=0.18)
    ap.add_argument("--iw-consistency", type=float, default=0.15)
    ap.add_argument("--iw-return", type=float, default=0.10)
    ap.add_argument("--iw-vol", type=float, default=0.12)
    ap.add_argument("--iw-horizon", type=float, default=0.15)
    ap.add_argument("--iw-mdd", type=float, default=0.15)
    ap.add_argument("--iw-vol-spot", type=float, default=0.15)
    args = ap.parse_args()

    opt = _load_opt_module()

    if args.json_path == "-":
        data: dict[str, Any] = json.load(sys.stdin)
    else:
        data = json.loads(Path(args.json_path).read_text(encoding="utf-8"))

    sc, bd, inv = opt.score_run_interpretable(
        data,
        args.horizon,
        args.max_vol_skip_pct,
        args.iw_sharpe,
        args.iw_consistency,
        args.iw_return,
        args.iw_vol,
        args.iw_horizon,
        args.iw_mdd,
        args.iw_vol_spot,
        args.min_wf_oos_trades,
    )

    horizons = data.get("horizons") or []
    h = opt.pick_horizon(horizons, args.horizon)
    wf = h.get("walk_forward") or {}
    bt = h.get("backtest") or {}

    print("=== Skor seçilen ufuk (primary) ===")
    print(f"  horizon_days={h.get('horizon_days')}  candles_used={h.get('candles_used')}")
    if not math.isfinite(sc):
        print(f"  GEÇERSİZ skor: {inv}")
        return 1

    print(f"  composite_0_100 = {sc:.4f}\n")

    # Ufuk robust detayı (7/14/30 WF OOS Sharpe)
    print("=== Ufuk robust (min( sharpe_0_100(7g), 14g, 30g ) ) ===")
    for d in (7, 14, 30):
        hh = next((x for x in horizons if x.get("horizon_days") == d), None)
        if not hh:
            print(f"  {d}g: yok")
            continue
        wfx = hh.get("walk_forward") or {}
        err = wfx.get("error")
        if err:
            print(f"  {d}g: WF hata — {str(err)[:80]}")
            continue
        sh = float(wfx.get("avg_test_sharpe") or 0.0)
        s100 = opt.sharpe_to_0_100(sh)
        print(f"  {d}g: wf_oos_Sharpe={sh:.6f}  →  comp={s100:.2f}/100")
    h100 = opt.multi_horizon_robustness_0_100(data)
    print(f"  → comp_horizon_robust_0_100 = min = {h100:.4f}\n")

    def fmt_raw(key: str, prec: int = 4) -> str:
        v = bd.get(key)
        if v is None:
            return "—"
        if prec == 6:
            return f"{float(v):.6f}"
        return f"{float(v):.{prec}f}"

    rows: list[tuple[str, str, str, str]] = [
        ("WF OOS Sharpe (primary)", "wf_oos_sharpe_raw", "comp_sharpe_0_100", "norm_weight_sharpe"),
        ("Tutarlılık", "wf_consistency_raw", "comp_consistency_0_100", "norm_weight_consistency"),
        ("WF OOS getiri %", "wf_oos_return_pct_raw", "comp_return_0_100", "norm_weight_return"),
        ("Vol rejim skip %", "vol_filter_skip_pct", "comp_vol_regime_0_100", "norm_weight_vol_regime"),
        ("Ufuk robust (min)", "—", "comp_horizon_robust_0_100", "norm_weight_horizon"),
        ("Backtest MDD %", "bt_max_drawdown_pct_raw", "comp_mdd_0_100", "norm_weight_mdd"),
        ("Spot hacim düşük %", "bt_volume_low_pct_of_signal_attempts_raw", "comp_spot_volume_0_100", "norm_weight_spot_volume"),
    ]

    print(
        f"=== Bileşenler (primary={args.horizon}g: WF + aynı ufuktaki backtest; "
        f"ufuk robust ayrıca 7/14/30g WF Sharpe) ==="
    )
    print(f"  {'bileşen':<26} {'ham':>14} {'0–100':>10} {'ağırlık':>10} {'katkı w×c':>12}")
    print("  " + "-" * 76)

    total = 0.0
    contribs: list[tuple[str, float]] = []
    for label, raw_k, comp_k, wk in rows:
        if raw_k == "—":
            raw_s = "—"
        elif raw_k == "wf_oos_sharpe_raw":
            raw_s = fmt_raw("wf_oos_sharpe_raw", 6)
        else:
            raw_s = fmt_raw(raw_k, 4)

        c = float(bd.get(comp_k, 0))
        w = float(bd.get(wk, 0))
        contrib = w * c
        total += contrib
        contribs.append((label, contrib))
        print(f"  {label:<26} {raw_s:>14} {c:>10.2f} {w:>10.4f} {contrib:>12.4f}")

    print("  " + "-" * 76)
    print(f"  {'TOPLAM (composite)':<26} {'':>14} {'':>10} {'':>10} {total:>12.4f}")

    # En çok kesen: en düşük comp_0_100 veya en düşük katkı
    comps = [
        ("Sharpe", float(bd.get("comp_sharpe_0_100", 0))),
        ("Tutarlılık", float(bd.get("comp_consistency_0_100", 0))),
        ("Getiri", float(bd.get("comp_return_0_100", 0))),
        ("Vol rejim", float(bd.get("comp_vol_regime_0_100", 0))),
        ("Ufuk robust", float(bd.get("comp_horizon_robust_0_100", 0))),
        ("MDD", float(bd.get("comp_mdd_0_100", 0))),
        ("Spot hacim", float(bd.get("comp_spot_volume_0_100", 0))),
    ]
    min_name, min_v = min(comps, key=lambda x: x[1])
    min_contrib = min(contribs, key=lambda x: x[1])
    print("\n=== Özet ===")
    print(f"  En düşük 0–100 bileşen: {min_name} = {min_v:.2f}  (skor tavanını çeken aday)")
    print(f"  En düşük ağırlıklı katkı: {min_contrib[0]} → {min_contrib[1]:.4f}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
