# polymarket-llm-bot

Polymarket tahmin pazarlarında (ör. BTC/ETH, 5m) **teknik analiz** tabanlı sinyal üreten ve CLOB üzerinden emir gönderen Rust botu. Spot mum verisi (varsayılan Binance), Gamma API ile pazar seçimi, edge / Kelly ile pozisyon boyutu ve risk limitleri kullanılır.

## Crate yapısı

| Parça | Açıklama |
|-------|----------|
| **Kütüphane** `polymarket_llm_bot` | `trading_loop`, Gamma/spot/CLOB istemcileri, `signals` (RSI+momentum kümesi + MACD, spot hacim), `volatility` rejim filtresi, `edge`, `risk`, `indicator_cache`, `backtest` / `walk_forward` |
| **Binary** `main` | `.env`, OpenTelemetry (isteğe bağlı), Prometheus `/metrics` (isteğe bağlı), sonsuz tarama döngüsü |
| **`metrics`** modülü | JSONL dosya logları (`data/`; üretimde opsiyonel kullanım) |
| **`prometheus_export`** | Prometheus scrape: `GET /metrics` (ör. `127.0.0.1:9090`) |

## Veri akışı (özet)

```
Gamma API          → aktif pazarlar (likidite, soru metni)
Binance (spot)     → mumlar
        │
        ▼
Sinyal üretimi     → RSI + 5m/15m momentum → tek “küme” oyu; MACD çizgisi → ikinci oyu
                     (çelişirse MACD tie-breaker). İsteğe bağlı: VOLUME_MIN_RATIO altında veto.
                     Yüksek volume_ratio → güven +0.1 (tavan 0.95).
        │
        ▼
Volatilite rejimi  → VOL_MIN/MAX_STD_PCT (getiri std×100; yoksa kapalı)
        │
        ▼
TechnicalSignal    → olasılık + güven + yön (Up/Down)
        │
        ▼
market_matcher     → soru metnine göre YES / NO yönü
        │
        ▼
edge + Kelly       → min edge ve pozisyon USDC
        │
        ▼
RiskManager        → günlük kayıp limiti, pazar başına tek pozisyon
        │
        ▼
Execution (CLOB)   → FAK market order (dry-run veya canlı)
```

## `analyze` metrikleri (backtest JSON)

- `bars_with_signal`: teknik sinyalin üretildiği bar sayısı (hacim + net sinyal geçti).
- `volume_low_skips` / `no_clear_signal_skips`: düşük hacim veya net yön yok.
- `vol_filter_skips`: volatilite rejim filtresi.
- `volume_low_pct_of_signal_attempts`: tüm sinyal denemeleri içinde düşük hacim oranı.

## Grid arama (`scripts/optimize_analyze.py`)

Tek bir `analyze` JSON’unda interpretable skorunun satır satır dökümü: `./scripts/explain_interpretable_score.py dosya.json`. Çoklu env grid’i ile `cargo run --bin analyze -- --json` tekrarlanır. **Varsayılan skor** `interpretable`: 0–100 birleşik değer; bileşenler (WF OOS Sharpe, tutarlılık, OOS getiri, vol **rejim** atlama, 7/14/30g ufuk robustluğu, backtest **MDD %**, spot **düşük hacim** atlama %) ve `--iw-*` ağırlıkları (normalize). Eski skor: `--score-mode legacy`. `--json-out` interpretable iken `{ "optimize": …, "analyze": … }`; sadece analyze kökü için `--json-out-raw-analyze`. `--json-score-out` yalnızca skor dökümü. Varsayılan `--max-combinations` 10_000.

## Kurulum

```bash
cp env.example .env
# .env içinde en azından POLYMARKET_PRIVATE_KEY ve strateji değişkenlerini ayarla

cargo build --release
cargo run --release
# (varsayılan binary: polymarket-llm-bot — analyze/research için: --bin analyze | --bin research)
```

## Dry run (varsayılan)

`DRY_RUN=true` iken gerçek emir gönderilmez; karar ve loglar üzerinden davranışı doğrulayabilirsin.

## Canlı işlem

1. `DRY_RUN=false`
2. [CLOB / imza ayarları](#polymarket-sdk-ve-clob) doğru olsun
3. Küçük bakiye ile test et

## Gözlemlenebilirlik (isteğe bağlı)

### OpenTelemetry (OTLP)

Trace’leri Jaeger / collector’a göndermek için örnek:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317
export OTEL_SERVICE_NAME=polymarket-llm-bot
# Jaeger all-in-one: COLLECTOR_OTLP_ENABLED=true, 4317 gRPC
```

Ctrl+C ile süreç çıkarken tracer kapatılır (bekleyen span’lar flush).

### JSON log

Log satırlarını JSON için: `LOG_JSON=true`.

### Prometheus

```bash
METRICS_ENABLED=true          # false / 0 ile kapat
METRICS_BIND=127.0.0.1:9090   # scrape adresi
# curl -s http://127.0.0.1:9090/metrics
```

Örnek metrikler: `trades_total`, `orders_failed_total`, `cycle_duration_seconds`, `markets_scanned_total`.

## Polymarket SDK ve CLOB

Proje **polymarket-client-sdk** (CLOB, Gamma, CTF) kullanır.

### Funder (EOA / Proxy / Gnosis Safe)

- **EOA**: Funder cüzdanın kendisidir; `FUNDER_ADDRESS` kullanılmaz.
- **Proxy / Gnosis Safe**: SDK CREATE2 ile funder türetebilir; isteğe bağlı `FUNDER_ADDRESS` ile override.

```bash
SIGNATURE_TYPE=GNOSIS_SAFE
POLYMARKET_PRIVATE_KEY=0x...
# FUNDER_ADDRESS boş bırakılabilir (otomatik türetim)

# veya manuel:
# FUNDER_ADDRESS=0xYourSafeAddress
```

### Sabitler ve env

- Zincir kimliği ve `POLYMARKET_PRIVATE_KEY` ismi SDK ile uyumludur (`config` modülü).
- Emir boyutu: SDK `Amount::shares()` ile; imza tipleri `SIGNATURE_TYPE` ile (EOA / PROXY / GNOSIS_SAFE veya sayısal).

### Builder API (opsiyonel)

Market maker / düşük gecikme senaryoları için env ile verilir; normal işlem için zorunlu değildir.

```bash
BUILDER_API_KEY=...
BUILDER_API_SECRET=...
BUILDER_API_PASSPHRASE=...
```

Kimlik bilgileri yoksa yalnızca standart kimlik doğrulanmış CLOB istemcisi kullanılır; tanımlıysa `promote_to_builder` ile Builder istemcisine yükseltilir.

## Önemli ortam değişkenleri

| Değişken | Varsayılan (özet) | Açıklama |
|----------|-------------------|----------|
| `ASSETS` | btc,eth | Gamma’da taranacak varlıklar |
| `DURATIONS` | 5m | Pazar süre filtreleri |
| `MIN_EDGE` | 0.06 | Teknik olasılık ile piyasa fiyatı arasında minimum fark |
| `MIN_CONFIDENCE` | 0.70 | Teknik sinyal güven eşiği (0.5–1.0) |
| `MIN_ORDER_USDC` | 5 | Minimum emir USDC |
| `CANDLE_INTERVAL` / `CANDLE_LOOKBACK` | 1m / 100 | Mum kaynağı ve derinlik |
| `RSI_PERIOD`, `MACD_*` | 14, 12, 26, 9 | Gösterge periyotları |
| `VOLUME_MIN_RATIO` | (yok) | Son mum / ortalama hacim altındaysa sinyal yok (ör. `0.7`) |
| `VOLUME_AVG_BARS` | 20 | Hacim ortalaması için mum sayısı |
| `VOL_*` | bkz. `env.example` | Getiri volatilitesi rejim filtresi |
| `MAX_POSITION_PCT` | 0.05 | İşlem başına bakiye üst sınırı |
| `DAILY_LOSS_LIMIT_PCT` | 0.10 | Günlük kayıp limiti (bakiye oranı) |
| `INITIAL_BALANCE` | 200 | Risk hesapları için başlangıç |
| `CYCLE_SECS` | 60 | Tarama periyodu (saniye) |

Tam liste ve yorumlar için `env.example` dosyasına bak.

## Kalibrasyon ve geliştirme

- `DRY_RUN=true` ile uzun süre çalıştırıp loglardaki atlama nedenlerini (düşük likidite, düşük güven, küçük edge) incele.
- `MIN_EDGE`, `MIN_CONFIDENCE`, RSI/MACD periyotları ve `CANDLE_*` değerlerini kendi verin için ayarla.
- **Offline araştırma CLI** (`research` binary): Binance’tan mum çekip backtest veya walk-forward çalıştırır.

```bash
cargo run --bin research -- backtest --asset btc --interval 1m --limit 500
cargo run --bin research -- walk-forward --asset btc --limit 1000 --train-window 400 --test-window 300
cargo run --bin analyze -- --asset btc --interval 1m
```

`analyze`: 1 / 7 / 14 / 30 günlük pencerelerde backtest + walk-forward özetleri ve kural tabanlı parametre önerileri (JSON: `--json`). Proje kökündeki `.env` yüklenir; `MIN_EDGE`, `CANDLE_INTERVAL`, `VOLUME_MIN_RATIO`, `VOL_MAX_STD_PCT` vb. **mevcut değerleriniz** hem backtest’e hem “şimdiki” sütununa yansır. İnsan okunur çıktıda vol ve hacim satırları özetlenir.

Tam çıktı için `--json`. Yardım: `cargo run --bin research -- --help`. Binance’ta 1000’den fazla mum istendiğinde istemci `endTime` ile sayfalama yapar.

## Lisans ve sorumluluk

Bu yazılım yatırım tavsiyesi değildir. Canlı işlem öncesi riskleri kendin değerlendir; kayıplardan proje sorumlu tutulamaz.
