#!/usr/bin/env python3
"""Analysis 7: summarize candle/HTF settings from config.toml and scan repo for candle_interval."""

import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONFIG = ROOT / "config.toml"


def main():
    print("=== 7. Multi-timeframe / candles (from config.toml) ===\n")
    text = CONFIG.read_text()
    for label, pat in [
        ("candle_interval", r'candle_interval\s*=\s*"([^"]+)"'),
        ("candle_lookback (global [technical])", r"\[technical\][\s\S]*?candle_lookback\s*=\s*(\d+)"),
        ("htf interval", r"\[htf\][\s\S]*?interval\s*=\s*\"([^\"]+)\""),
        ("htf lookback", r"\[htf\][\s\S]*?lookback\s*=\s*(\d+)"),
        ("htf ema_period", r"\[htf\][\s\S]*?ema_period\s*=\s*(\d+)"),
    ]:
        m = re.search(pat, text)
        if m:
            print(f"  {label}: {m.group(1)}")

    print("\nPer-asset candle_lookback overrides (from config.toml):")
    cur = None
    for line in text.splitlines():
        m = re.match(r"\[asset\.(\w+)\]", line)
        if m:
            cur = m.group(1)
            continue
        if cur and "candle_lookback" in line:
            print(f"  {cur}: {line.strip()}")

    print("\n=== All `candle_interval` references (grep, excluding target/) ===\n")
    try:
        out = subprocess.check_output(
            [
                "grep", "-rn", "candle_interval",
                str(ROOT / "src"),
                str(ROOT / "config.toml"),
            ],
            stderr=subprocess.DEVNULL,
            text=True,
        )
        for line in out.splitlines():
            print(line)
    except subprocess.CalledProcessError as e:
        print("(no matches)" if e.returncode == 1 else e)


if __name__ == "__main__":
    main()
