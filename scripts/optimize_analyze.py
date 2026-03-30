#!/usr/bin/env python3
"""
Grid search over env vars, repeatedly runs `cargo run --bin analyze -- --json`, scores JSON output.

Öncelik: proje kökünden çalıştırın (`.env` yüklensin). Deneme değişkenleri subprocess ortamında
set edilir; dotenv mevcut env'i ezmez, bu yüzden denemeler `.env` üzerine yazılır.

Ne optimize edilebilir? (`src/bin/analyze.rs` → `load_env_strategy`)

  MIN_EDGE, MIN_CONFIDENCE — CANDLE_LOOKBACK — RSI_PERIOD, MACD_FAST, MACD_SLOW, MACD_SIGNAL
  VOL_MAX_STD_PCT, VOL_MIN_STD_PCT, VOL_SAMPLE_BARS — VOLUME_MIN_RATIO, VOLUME_AVG_BARS
  — (isteğe bağlı CANDLE_INTERVAL)

  Tek varlıkta global `MIN_EDGE` yeterli; `.env`'de aynı anda `MIN_EDGE` ve `MIN_EDGE_BTC`
  tanımlıysa önce asset-sonekli okunur — script yalnızca set ettiğiniz anahtarları gönderir.

Kısıtlar (geçersiz kombinasyonlar çalıştırılmaz): MACD_FAST < MACD_SLOW; 5<=RSI<=50;
  CANDLE_LOOKBACK>=50; 5<=VOL_SAMPLE_BARS<=500; 0<VOLUME_MIN_RATIO<=5; 5<=VOLUME_AVG_BARS<=200.
  analyze 1g ufku için mum sayısı = 1 günlük bar (5m’de ~288): CANDLE_LOOKBACK + holding
  bu sınırı aşmamalı (ör. 5m + holding=5 → lookback <= 283).

Script, `--asset btc` iken yukarıdaki anahtarları otomatik `MIN_EDGE_BTC` vb. yapar;
  böylece `.env` içindeki `MIN_EDGE_BTC` deneme değerinin üzerine yazılır.

Kombinasyon sayısı = eksen değerlerinin çarpanı. `multi` ~5.7k, `wide` ~60k geçerli
  kombinasyon; --max-combinations buna göre ayarlayın (wide için 60000+).

Örnekler:
  ./scripts/optimize_analyze.py --quick
  ./scripts/optimize_analyze.py --preset multi
  ./scripts/optimize_analyze.py --preset wide --max-combinations 60000
  ./scripts/optimize_analyze.py --grid 'VOL_MAX_STD_PCT:0.28,0.35;MIN_EDGE:0.13,0.15;RSI_PERIOD:24,28'

Skor modları (`--score-mode`):
  - interpretable (varsayılan): 0–100 birleşik skor; bileşenler 0–100 ve normalize ağırlıklı ortalama.
    • WF OOS Sharpe, tutarlılık, OOS getiri %, volatilite rejiminde atlama kalitesi,
      7/14/30g ufuk robustluğu, backtest max drawdown %, spot düşük hacim atlama %.
    • `--json-out`: interpretable iken tek dosyada { "optimize": …, "analyze": … }.
  - legacy: eski birim yok skor (Sharpe + consistency + getiri terimi) × vol_pen.

İşlem / WF yoksa veya vol atlama çok yüksekse skor geçersiz (-inf / seçilemez).
"""

from __future__ import annotations

import argparse
import itertools
import json
import math
import os
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


def parse_grid(spec: str) -> dict[str, list[str]]:
    """'KEY:a,b;KEY2:c' -> {KEY: [a,b], KEY2: [c]}"""
    out: dict[str, list[str]] = {}
    for part in spec.split(";"):
        part = part.strip()
        if not part:
            continue
        if ":" not in part:
            raise ValueError(f"Grid parçası geçersiz (KEY:v1,v2 beklenir): {part!r}")
        key, vals = part.split(":", 1)
        key = key.strip()
        out[key] = [v.strip() for v in vals.split(",") if v.strip()]
    if not out:
        raise ValueError("Boş grid")
    return out


def default_quick_grid() -> dict[str, list[str]]:
    return {
        "VOL_MAX_STD_PCT": ["0.15", "0.22", "0.28", "0.35", "0.45"],
    }


def candles_per_day_for_interval(interval: str) -> int:
    """`src/bin/analyze.rs` `candles_per_day` ile aynı (1 günlük bar sayısı)."""
    iv = interval.strip().lower()
    if iv == "1m":
        return 60 * 24
    if iv == "3m":
        return 60 * 24 // 3
    if iv == "5m":
        return 60 * 24 // 5
    if iv == "15m":
        return 60 * 24 // 15
    if iv == "30m":
        return 60 * 24 // 30
    if iv in ("1h", "60m"):
        return 24
    if iv == "4h":
        return 6
    if iv in ("1d", "1D"):
        return 1
    return 60 * 24


def default_multi_grid() -> dict[str, list[str]]:
    """Genişletilmiş eksenler (~5.7k geçerli kombinasyon, MACD fast<slow)."""
    return {
        "VOL_MAX_STD_PCT": ["0.22", "0.28", "0.32", "0.35", "0.38"],
        "MIN_EDGE": ["0.11", "0.13", "0.15", "0.17"],
        "MIN_CONFIDENCE": ["0.74", "0.78", "0.82", "0.86"],
        # 5m: 1g ≈288 mum — lookback+holding (varsayılan 5) <= 288 olmalı
        "CANDLE_LOOKBACK": ["200", "280"],
        "RSI_PERIOD": ["22", "24", "28"],
        "MACD_FAST": ["12"],
        "MACD_SLOW": ["26", "30", "34"],
        "MACD_SIGNAL": ["9", "12"],
        "VOL_SAMPLE_BARS": ["10", "20"],
    }


def default_wide_grid() -> dict[str, list[str]]:
    """Maksimum eksen çeşitliliği (~60k geçerli kombinasyon). Tam tarama için --max-combinations 60000+."""
    return {
        "VOL_MAX_STD_PCT": ["0.22", "0.25", "0.28", "0.32", "0.38"],
        "MIN_EDGE": ["0.09", "0.11", "0.13", "0.15", "0.17"],
        "MIN_CONFIDENCE": ["0.66", "0.70", "0.74", "0.78", "0.82"],
        "CANDLE_LOOKBACK": ["200", "250", "280"],
        "RSI_PERIOD": ["22", "24", "28", "32"],
        "MACD_FAST": ["12"],
        "MACD_SLOW": ["26", "28", "30", "34", "36"],
        "MACD_SIGNAL": ["9", "12"],
        "VOL_SAMPLE_BARS": ["10", "20", "40", "60"],
    }


def env_combo_valid(
    extra: dict[str, str],
    *,
    analyze_interval: str,
    holding_period: int,
) -> bool:
    """analyze + config kısıtları; MACD çifti setliyse fast < slow şartı."""
    if "MACD_FAST" in extra and "MACD_SLOW" in extra:
        try:
            if int(extra["MACD_FAST"]) >= int(extra["MACD_SLOW"]):
                return False
        except ValueError:
            return False
    if "RSI_PERIOD" in extra:
        try:
            r = int(extra["RSI_PERIOD"])
            if r < 5 or r > 50:
                return False
        except ValueError:
            return False
    if "CANDLE_LOOKBACK" in extra:
        try:
            lb = int(extra["CANDLE_LOOKBACK"])
            if lb < 50:
                return False
            # analyze en kısa ufukta 1g mum kullanır; backtest için lb + holding <= o dilimdeki bar
            eff_iv = (extra.get("CANDLE_INTERVAL") or analyze_interval).strip()
            per_1d = candles_per_day_for_interval(eff_iv)
            if lb + holding_period > per_1d:
                return False
        except ValueError:
            return False
    if "VOL_SAMPLE_BARS" in extra:
        try:
            v = int(extra["VOL_SAMPLE_BARS"])
            if v < 5 or v > 500:
                return False
        except ValueError:
            return False
    if "VOLUME_MIN_RATIO" in extra:
        try:
            r = float(extra["VOLUME_MIN_RATIO"])
            if r <= 0.0 or r > 5.0:
                return False
        except ValueError:
            return False
    if "VOLUME_AVG_BARS" in extra:
        try:
            b = int(extra["VOLUME_AVG_BARS"])
            if b < 5 or b > 200:
                return False
        except ValueError:
            return False
    return True


def expand_trials(
    grid: dict[str, list[str]],
    *,
    analyze_interval: str,
    holding_period: int,
) -> list[dict[str, str]]:
    keys = list(grid.keys())
    values = [grid[k] for k in keys]
    out: list[dict[str, str]] = []
    for combo in itertools.product(*values):
        extra = dict(zip(keys, combo))
        if env_combo_valid(
            extra,
            analyze_interval=analyze_interval,
            holding_period=holding_period,
        ):
            out.append(extra)
    return out


# `analyze` önce `MIN_EDGE_BTC` okur, sonra `MIN_EDGE`. .env'de sonekli değer varsa
# yalnız global anahtar göndermek denemeyi devre dışı bırakır.
_ASSET_SUFFIX_KEYS = frozenset(
    {
        "MIN_EDGE",
        "MIN_CONFIDENCE",
        "CANDLE_LOOKBACK",
        "RSI_PERIOD",
        "MACD_FAST",
        "MACD_SLOW",
        "MACD_SIGNAL",
        "VOL_MAX_STD_PCT",
        "VOL_MIN_STD_PCT",
        "VOL_SAMPLE_BARS",
        "VOLUME_MIN_RATIO",
        "VOLUME_AVG_BARS",
        "CANDLE_INTERVAL",
    }
)


def apply_asset_env_suffix(extra: dict[str, str], asset: str) -> dict[str, str]:
    au = asset.strip().upper()
    if not au:
        return extra
    suf = f"_{au}"
    out: dict[str, str] = {}
    for k, v in extra.items():
        if k in _ASSET_SUFFIX_KEYS:
            out[k + suf] = v
        else:
            out[k] = v
    return out


def run_analyze(
    project_root: Path,
    asset: str,
    interval: str,
    extra_env: dict[str, str],
    release: bool,
    holding_period: int,
) -> dict[str, Any]:
    cmd = ["cargo", "run"]
    if release:
        cmd.append("--release")
    cmd += [
        "--bin",
        "analyze",
        "--",
        "--asset",
        asset,
        "--interval",
        interval,
        "--holding-period",
        str(holding_period),
        "--json",
    ]
    env = os.environ.copy()
    env.update(apply_asset_env_suffix(extra_env, asset))
    # analyze proje kökünde .env okur
    proc = subprocess.run(
        cmd,
        cwd=project_root,
        env=env,
        capture_output=True,
        text=True,
        timeout=600,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"analyze başarısız (exit {proc.returncode})\n{proc.stderr}\n{proc.stdout[:2000]}"
        )
    out = proc.stdout
    if not out.strip():
        raise RuntimeError("analyze boş stdout")
    # cargo "Finished / Running" satırları + JSON; kök { ilk olanı (rfind iç içe objeleri seçerdi)
    start = out.find("{")
    if start == -1:
        raise RuntimeError(f"JSON yok: {out[:500]}")
    decoder = json.JSONDecoder()
    data, _ = decoder.raw_decode(out, start)
    return data


def pick_horizon(horizons: list[dict[str, Any]], target_days: int) -> dict[str, Any]:
    for h in horizons:
        if h.get("horizon_days") == target_days:
            return h
    # en büyük horizon_days
    return max(horizons, key=lambda h: h.get("horizon_days", 0))


def _finite(x: float) -> bool:
    try:
        return math.isfinite(float(x))
    except (TypeError, ValueError):
        return False


def sharpe_to_0_100(sharpe: float) -> float:
    """WF OOS Sharpe → 0–100 (negatif / NaN → 0). ~1 iyi, ~2+ çok iyi."""
    if not _finite(sharpe) or sharpe <= 0:
        return 0.0
    s = float(sharpe)
    if s <= 1.0:
        return 35.0 * s
    if s <= 2.0:
        return 35.0 + 30.0 * (s - 1.0)
    if s <= 3.0:
        return 65.0 + 20.0 * (s - 2.0)
    return min(100.0, 85.0 + 5.0 * (s - 3.0))


def consistency_to_0_100(raw: float, iterations: int) -> float:
    """Rust consistency_score (tavan sınırsız) → 0–100; az iterasyonda güven cezası."""
    if not _finite(raw):
        return 0.0
    x = min(float(raw), 8.0)
    base = 100.0 * (1.0 - 1.0 / (1.0 + x))
    if iterations < 2:
        base *= 0.65
    return max(0.0, min(100.0, base))


def return_pct_to_0_100(ret_pct: float) -> float:
    """WF avg test return % (ör. 2.5 = %2.5) → 0–100; ~0% → 50."""
    if not _finite(ret_pct):
        return 50.0
    r = max(-25.0, min(25.0, float(ret_pct)))
    return max(0.0, min(100.0, 50.0 + 2.0 * r))


def vol_skip_to_quality_0_100(skip_pct: float) -> float:
    """vol_filter_skip_pct_of_signals: atlama az → skor yüksek."""
    if not _finite(skip_pct):
        return 50.0
    s = max(0.0, min(100.0, float(skip_pct)))
    return max(0.0, 100.0 - s)


def mdd_pct_to_0_100(mdd_pct: float) -> float:
    """Backtest max_drawdown_pct (pozitif %, peak’e göre) → 0–100; düşük MDD = yüksek skor."""
    if not _finite(mdd_pct):
        return 50.0
    m = max(0.0, float(mdd_pct))
    # 0% → 100; 50% drawdown → 0 (doğrusal kırpma)
    return max(0.0, min(100.0, 100.0 - 2.0 * m))


def spot_volume_low_to_quality_0_100(volume_low_pct: float) -> float:
    """volume_low_pct_of_signal_attempts: düşük hacim atlama az → skor yüksek."""
    if not _finite(volume_low_pct):
        return 50.0
    s = max(0.0, min(100.0, float(volume_low_pct)))
    return max(0.0, 100.0 - s)


def multi_horizon_robustness_0_100(data: dict[str, Any]) -> float:
    """7g / 14g / 30g WF OOS Sharpe skorlarının minimumu (en zayıf ufuk)."""
    horizons = data.get("horizons") or []
    vals: list[float] = []
    for d in (7, 14, 30):
        h = next((x for x in horizons if x.get("horizon_days") == d), None)
        if not h:
            continue
        wf = h.get("walk_forward") or {}
        if wf.get("error"):
            continue
        sh = float(wf.get("avg_test_sharpe") or 0.0)
        if not _finite(sh):
            continue
        vals.append(sharpe_to_0_100(sh))
    if not vals:
        return 50.0
    return min(vals)


def _normalize_weights(*w: float) -> tuple[float, ...]:
    s = sum(max(0.0, float(x)) for x in w)
    if s <= 0:
        n = len(w)
        return tuple(1.0 / n for _ in range(n))
    return tuple(max(0.0, float(x)) / s for x in w)


def score_run_interpretable(
    data: dict[str, Any],
    horizon_days: int,
    max_vol_skip_pct: float,
    w_s: float,
    w_c: float,
    w_r: float,
    w_v: float,
    w_h: float,
    w_mdd: float,
    w_vs: float,
    min_wf_oos_trades: int,
) -> tuple[float, dict[str, float], str | None]:
    """0–100 birleşik skor + bileşenler (0–100). Geçersiz: (-inf, {}, kısa neden)."""
    horizons = data.get("horizons") or []
    if not horizons:
        return float("-inf"), {}, "horizon_yok"
    h = pick_horizon(horizons, horizon_days)
    wf = h.get("walk_forward") or {}
    bt = h.get("backtest") or {}
    if wf.get("error"):
        err = wf.get("error")
        msg = str(err)[:120] if err else ""
        return float("-inf"), {}, f"wf_hata:{msg}"
    if bt.get("trades", 0) == 0:
        return float("-inf"), {}, "backtest_islem_yok"
    oos_trades = int(wf.get("total_test_trades") or 0)
    if oos_trades == 0:
        return float("-inf"), {}, "wf_oos_islem_yok"
    if oos_trades < min_wf_oos_trades:
        return (
            float("-inf"),
            {},
            f"wf_oos_islem_az({oos_trades}<{min_wf_oos_trades})",
        )

    vol_skip = float(bt.get("vol_filter_skip_pct_of_signals") or 0.0)
    if vol_skip >= max_vol_skip_pct:
        return (
            float("-inf"),
            {},
            f"vol_rejim_atlama_yuksek({vol_skip:.1f}>={max_vol_skip_pct})",
        )

    sharpe = float(wf.get("avg_test_sharpe") or 0.0)
    ret = float(wf.get("avg_test_return_pct") or 0.0)
    cons = float(wf.get("consistency_score") or 0.0)
    iters = int(wf.get("iterations") or 0)

    s100 = sharpe_to_0_100(sharpe)
    c100 = consistency_to_0_100(cons, iters)
    r100 = return_pct_to_0_100(ret)
    v100 = vol_skip_to_quality_0_100(vol_skip)
    h100 = multi_horizon_robustness_0_100(data)

    mdd_pct = float(bt.get("max_drawdown_pct") or 0.0)
    m100 = mdd_pct_to_0_100(mdd_pct)
    vol_low_pct = float(bt.get("volume_low_pct_of_signal_attempts") or 0.0)
    vs100 = spot_volume_low_to_quality_0_100(vol_low_pct)

    ws, wc, wr, wv, wh, wm, wvs = _normalize_weights(w_s, w_c, w_r, w_v, w_h, w_mdd, w_vs)
    composite = (
        ws * s100
        + wc * c100
        + wr * r100
        + wv * v100
        + wh * h100
        + wm * m100
        + wvs * vs100
    )

    breakdown: dict[str, float] = {
        "composite_0_100": round(composite, 4),
        "wf_oos_sharpe_raw": round(sharpe, 6),
        "wf_oos_return_pct_raw": round(ret, 4),
        "wf_consistency_raw": round(cons, 6),
        "wf_iterations": float(iters),
        "wf_oos_trades": float(oos_trades),
        "vol_filter_skip_pct": round(vol_skip, 4),
        "bt_max_drawdown_pct_raw": round(mdd_pct, 4),
        "bt_volume_low_pct_of_signal_attempts_raw": round(vol_low_pct, 4),
        "comp_sharpe_0_100": round(s100, 4),
        "comp_consistency_0_100": round(c100, 4),
        "comp_return_0_100": round(r100, 4),
        "comp_vol_regime_0_100": round(v100, 4),
        "comp_horizon_robust_0_100": round(h100, 4),
        "comp_mdd_0_100": round(m100, 4),
        "comp_spot_volume_0_100": round(vs100, 4),
        "norm_weight_sharpe": round(ws, 4),
        "norm_weight_consistency": round(wc, 4),
        "norm_weight_return": round(wr, 4),
        "norm_weight_vol_regime": round(wv, 4),
        "norm_weight_horizon": round(wh, 4),
        "norm_weight_mdd": round(wm, 4),
        "norm_weight_spot_volume": round(wvs, 4),
    }
    return composite, breakdown, None


def score_run_legacy(
    data: dict[str, Any],
    horizon_days: int,
    w_sharpe: float,
    w_consistency: float,
    w_return: float,
    max_vol_skip_pct: float,
) -> tuple[float, dict[str, float], str | None]:
    """Eski birim yok skor; geçersiz = -inf."""
    horizons = data.get("horizons") or []
    if not horizons:
        return float("-inf"), {}, "horizon_yok"
    h = pick_horizon(horizons, horizon_days)
    wf = h.get("walk_forward") or {}
    bt = h.get("backtest") or {}
    if wf.get("error"):
        return float("-inf"), {}, "wf_hata"
    if bt.get("trades", 0) == 0:
        return float("-inf"), {}, "backtest_islem_yok"
    if wf.get("total_test_trades", 0) == 0:
        return float("-inf"), {}, "wf_oos_islem_yok"

    cons = float(wf.get("consistency_score") or 0.0)
    sharpe = float(wf.get("avg_test_sharpe") or 0.0)
    ret = float(wf.get("avg_test_return_pct") or 0.0)
    if sharpe < 0:
        sharpe = 0.0

    vol_skip = float(bt.get("vol_filter_skip_pct_of_signals") or 0.0)
    if vol_skip >= max_vol_skip_pct:
        return float("-inf"), {}, "vol_rejim_atlama_yuksek"

    vol_pen = 1.0
    if vol_skip > 85.0:
        vol_pen = max(0.15, 1.0 - (vol_skip - 85.0) / 15.0)

    raw = (
        w_consistency * cons
        + w_sharpe * min(sharpe, 25.0)
        + w_return * max(-10.0, min(ret, 50.0)) * 0.01
    ) * vol_pen
    bd = {
        "legacy_raw": round(raw, 6),
        "vol_penalty": round(vol_pen, 6),
    }
    return raw, bd, None


@dataclass
class TrialResult:
    score: float
    env: dict[str, str]
    raw: dict[str, Any]
    breakdown: dict[str, float] = field(default_factory=dict)


def main() -> int:
    ap = argparse.ArgumentParser(description="analyze grid search (JSON score)")
    ap.add_argument("--project-root", type=Path, default=Path(__file__).resolve().parent.parent)
    ap.add_argument("--asset", default="btc")
    ap.add_argument("--interval", default="5m")
    ap.add_argument(
        "--holding-period",
        type=int,
        default=5,
        help="analyze --holding-period (backtest ile grid doğrulaması; 1g ufku için lb+holding sınırı)",
    )
    ap.add_argument("--horizon", type=int, default=30, help="Skorda kullanılacak ufuk (gün)")
    ap.add_argument("--release", action="store_true", help="cargo --release")
    ap.add_argument("--quick", action="store_true", help="VOL_MAX_STD_PCT için kısa grid")
    ap.add_argument(
        "--preset",
        choices=["quick", "multi", "wide"],
        default=None,
        help="quick=vol-only; multi=~5.7k kombinasyon; wide=~60k ( --max-combinations 60000+ )",
    )
    ap.add_argument(
        "--grid",
        type=str,
        default="",
        help="Örn: 'VOL_MAX_STD_PCT:0.2,0.3;MIN_CONFIDENCE:0.78,0.80;RSI_PERIOD:22,28'",
    )
    ap.add_argument("--dry-run", action="store_true", help="Sadece kombinasyon sayısını yaz")
    ap.add_argument(
        "--max-combinations",
        type=int,
        default=10_000,
        help="Geçerli grid satırı üst sınırı (multi ~5760; wide için 60000+)",
    )
    ap.add_argument("--max-vol-skip-pct", type=float, default=92.0, help="Üstünde skor -inf")
    ap.add_argument(
        "--score-mode",
        choices=["interpretable", "legacy"],
        default="interpretable",
        help="interpretable=0-100 birleşik + bileşenler; legacy=eski skor birimi",
    )
    ap.add_argument(
        "--min-wf-oos-trades",
        type=int,
        default=5,
        help="interpretable: WF toplam OOS işlem altındaysa geçersiz (overfit riski)",
    )
    ap.add_argument("--w-sharpe", type=float, default=1.0, help="legacy: Sharpe terimi ağırlığı")
    ap.add_argument("--w-consistency", type=float, default=1.0, help="legacy: consistency ağırlığı")
    ap.add_argument("--w-return", type=float, default=0.5, help="legacy: getiri terimi ağırlığı")
    ap.add_argument(
        "--iw-sharpe",
        type=float,
        default=0.18,
        help="interpretable: WF OOS Sharpe bileşeni (normalize edilerek toplanır)",
    )
    ap.add_argument("--iw-consistency", type=float, default=0.15)
    ap.add_argument("--iw-return", type=float, default=0.10)
    ap.add_argument(
        "--iw-vol",
        type=float,
        default=0.12,
        help="interpretable: volatilite rejim filtresi (vol_filter_skip %%) bileşeni",
    )
    ap.add_argument("--iw-horizon", type=float, default=0.15, help="7/14/30g en zayıf ufuk ağırlığı")
    ap.add_argument(
        "--iw-mdd",
        type=float,
        default=0.15,
        help="interpretable: backtest max drawdown %% bileşeni",
    )
    ap.add_argument(
        "--iw-vol-spot",
        type=float,
        default=0.15,
        help="interpretable: spot düşük hacim atlama %% (volume_low_pct_of_signal_attempts)",
    )
    ap.add_argument(
        "--json-out",
        type=Path,
        default=None,
        help="Çıktı dosyası: interpretable iken {optimize, analyze}; --json-out-raw-analyze ile sadece analyze kökü",
    )
    ap.add_argument(
        "--json-out-raw-analyze",
        action="store_true",
        help="--json-out içinde birleştirme yok; yalnızca analyze JSON (eski davranış)",
    )
    ap.add_argument(
        "--json-score-out",
        type=Path,
        default=None,
        help="interpretable: en iyi skor dökümü (composite + bileşenler) JSON",
    )
    args = ap.parse_args()

    if args.grid.strip():
        grid = parse_grid(args.grid)
    elif args.preset == "multi":
        grid = default_multi_grid()
    elif args.preset == "wide":
        grid = default_wide_grid()
    elif args.preset == "quick" or args.quick:
        grid = default_quick_grid()
    else:
        grid = default_quick_grid()

    keys = list(grid.keys())
    values = [grid[k] for k in keys]
    raw_n = len(list(itertools.product(*values)))
    trials = expand_trials(
        grid,
        analyze_interval=args.interval,
        holding_period=args.holding_period,
    )
    if len(trials) > args.max_combinations:
        print(
            f"Çok fazla geçerli kombinasyon: {len(trials)} (ham çarpan {raw_n}) > {args.max_combinations}. "
            "Grid'i daraltın veya --max-combinations artırın.",
            file=sys.stderr,
        )
        return 2

    print(f"Grid: {grid}")
    print(
        f"Ham çarpan: {raw_n} | MACD/RSI/1g-lookback/… sonrası geçerli: {len(trials)} "
        f"(interval={args.interval!r}, holding={args.holding_period})"
    )
    if args.dry_run:
        return 0

    best: TrialResult | None = None
    for i, extra in enumerate(trials, 1):
        print(f"[{i}/{len(trials)}] deneme env: {extra}", flush=True)
        try:
            raw = run_analyze(
                args.project_root,
                args.asset,
                args.interval,
                extra,
                args.release,
                args.holding_period,
            )
            if args.score_mode == "interpretable":
                sc, bd, inv_reason = score_run_interpretable(
                    raw,
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
            else:
                sc, bd, inv_reason = score_run_legacy(
                    raw,
                    args.horizon,
                    args.w_sharpe,
                    args.w_consistency,
                    args.w_return,
                    args.max_vol_skip_pct,
                )
        except Exception as e:
            print(f"  HATA: {e}", flush=True)
            continue
        if not math.isfinite(sc):
            why = inv_reason or "bilinmiyor"
            print(f"  skor=geçersiz — {why}", flush=True)
        elif args.score_mode == "interpretable":
            print(
                f"  skor={sc:.2f}/100  "
                f"S={bd.get('comp_sharpe_0_100', 0):.0f} "
                f"Tut={bd.get('comp_consistency_0_100', 0):.0f} "
                f"Ret={bd.get('comp_return_0_100', 0):.0f} "
                f"VolR={bd.get('comp_vol_regime_0_100', 0):.0f} "
                f"Ufuk={bd.get('comp_horizon_robust_0_100', 0):.0f} "
                f"MDD={bd.get('comp_mdd_0_100', 0):.0f} "
                f"Spot={bd.get('comp_spot_volume_0_100', 0):.0f} "
                f"(wf_S={bd.get('wf_oos_sharpe_raw', 0):.3f} mdd%={bd.get('bt_max_drawdown_pct_raw', 0):.1f})",
                flush=True,
            )
        else:
            print(f"  skor={sc:.6f} (legacy)", flush=True)
        if math.isfinite(sc) and (best is None or sc > best.score):
            best = TrialResult(score=sc, env=extra, raw=raw, breakdown=bd)

    print()
    print("=== Sonuç ===")
    if not trials:
        print(
            "Geçerli kombinasyon yok (MACD fast≥slow, RSI/lookback aralığı veya "
            "CANDLE_LOOKBACK + holding > 1 günlük bar sayısı — interval ile uyumlu grid kullanın).",
            file=sys.stderr,
        )
        return 2
    if best is None:
        print("Hiç başarılı koşu yok.")
        return 1
    if args.score_mode == "interpretable":
        print(f"En iyi skor: {best.score:.2f}/100 (interpretable)")
        if best.breakdown:
            print("Bileşen özeti (0–100):", end="")
            for label, key in (
                ("Sharpe", "comp_sharpe_0_100"),
                ("Tutarlılık", "comp_consistency_0_100"),
                ("Getiri", "comp_return_0_100"),
                ("Vol rejim", "comp_vol_regime_0_100"),
                ("Ufuk robust", "comp_horizon_robust_0_100"),
                ("MDD", "comp_mdd_0_100"),
                ("Spot hacim", "comp_spot_volume_0_100"),
            ):
                print(f" {label}={best.breakdown.get(key, 0):.1f}", end="")
            print()
    else:
        print(f"En iyi skor: {best.score:.6f} (legacy, birim yok)")
    print("Önerilen ek .env satırları (mevcut .env ile birleştirin):")
    for k, v in sorted(apply_asset_env_suffix(best.env, args.asset).items()):
        print(f"  {k}={v}")
    if args.json_out:
        if (
            args.score_mode == "interpretable"
            and best.breakdown
            and not args.json_out_raw_analyze
        ):
            merged = {
                "optimize": {
                    "score_mode": "interpretable",
                    "primary_horizon_days": args.horizon,
                    "min_wf_oos_trades": args.min_wf_oos_trades,
                    "weights": {
                        "iw_sharpe": args.iw_sharpe,
                        "iw_consistency": args.iw_consistency,
                        "iw_return": args.iw_return,
                        "iw_vol_regime": args.iw_vol,
                        "iw_horizon": args.iw_horizon,
                        "iw_mdd": args.iw_mdd,
                        "iw_vol_spot": args.iw_vol_spot,
                    },
                    "best_env": apply_asset_env_suffix(best.env, args.asset),
                    "breakdown": best.breakdown,
                },
                "analyze": best.raw,
            }
            args.json_out.write_text(json.dumps(merged, indent=2, ensure_ascii=False))
            print(f"Birleşik JSON (optimize + analyze): {args.json_out}")
        else:
            args.json_out.write_text(json.dumps(best.raw, indent=2, ensure_ascii=False))
            print(f"Tam JSON (analyze): {args.json_out}")
    if args.json_score_out and args.score_mode == "interpretable" and best.breakdown:
        payload = {
            "score_mode": "interpretable",
            "primary_horizon_days": args.horizon,
            "min_wf_oos_trades": args.min_wf_oos_trades,
            "weights": {
                "iw_sharpe": args.iw_sharpe,
                "iw_consistency": args.iw_consistency,
                "iw_return": args.iw_return,
                "iw_vol_regime": args.iw_vol,
                "iw_horizon": args.iw_horizon,
                "iw_mdd": args.iw_mdd,
                "iw_vol_spot": args.iw_vol_spot,
            },
            "best_env": apply_asset_env_suffix(best.env, args.asset),
            "breakdown": best.breakdown,
        }
        args.json_score_out.write_text(json.dumps(payload, indent=2, ensure_ascii=False))
        print(f"Skor dökümü JSON: {args.json_score_out}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
