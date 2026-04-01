# polymarket-llm-bot

Polymarket tahmin pazarlarında (ör. BTC/ETH, 5m) **teknik analiz** tabanlı sinyal üreten ve CLOB üzerinden emir gönderen Rust botu. Spot mum verisi (varsayılan Binance), Gamma API ile pazar seçimi, edge / Kelly ile pozisyon boyutu ve risk limitleri kullanılır.

## Crate yapısı

| Parça | Açıklama |
|-------|----------|
| **Kütüphane** `polymarket_llm_bot` | `trading_loop`, Gamma/spot/CLOB istemcileri, `signals` (RSI+momentum kümesi + MACD, spot hacim), `volatility` rejim filtresi, `edge`, `risk`, `indicator_cache`, `resolution_checker` |
| **Binary** `main` | `.env`, OpenTelemetry (isteğe bağlı), Prometheus `/metrics` (isteğe bağlı), sonsuz tarama döngüsü |
| **`metrics`** modülü | JSONL: `DATA_DIR/trades.jsonl` (işlem + çözüm sonrası `outcome` / `pnl` / `resolved_at`), `skip_reasons.jsonl`, `order_failures.jsonl` (CLOB hataları) |
| **`stats` binary** | `cargo run --bin stats` — `trades.jsonl` üzerinde win rate, edge/confidence bucket, PnL özeti |
| **`prometheus_export`** | Prometheus scrape: `GET /metrics` (ör. `127.0.0.1:9090`) |

## Veri akışı (özet)

```
Gamma API          → aktif pazarlar (likidite, soru metni)
Binance (spot)     → mumlar
        │
        ▼
Sinyal üretimi     → Wilder RSI + 5m/15m momentum → küme oyu; MACD histogram (signal line EMA) + isteğe bağlı histogram crossover notu.
                     DOWN yönünde YES-olasılığı düşürülür (edge ile tutarlı). Binance taker buy ratio ile hizalı akışta güven +0.05.
                     İsteğe bağlı: `HTF_*` ile üst zaman dilimi (ör. 15m) EMA trend filtresi; `ADAPTIVE_THRESHOLDS` ile son trade win rate’e göre min_edge/min_confidence.
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

## Kurulum

```bash
cp env.example .env
# .env içinde en azından POLYMARKET_PRIVATE_KEY ve strateji değişkenlerini ayarla

cargo build --release
cargo run --release
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
| `DATA_DIR` | `data` | JSONL log dizini (`trades.jsonl`, `skip_reasons.jsonl`, `order_failures.jsonl`) |
| `HTF_ENABLED` / `HTF_INTERVAL` / `HTF_LOOKBACK` / `HTF_EMA_PERIOD` | false / 15m / 50 / 20 | Üst trend filtresi (`HTF_ENABLED_BTC` vb. asset override) |
| `ADAPTIVE_THRESHOLDS` / `ADAPTIVE_TRADE_WINDOW` | false / 50 | Son N çözülmüş trade win rate’e göre `min_edge` / `min_confidence` ayarı |

```bash
cargo run --bin stats -- --data-dir data
```

Tam liste ve yorumlar için `env.example` dosyasına bak.

## Kalibrasyon ve geliştirme

- `DRY_RUN=true` ile uzun süre çalıştırıp loglardaki atlama nedenlerini (düşük likidite, düşük güven, küçük edge) incele.
- `MIN_EDGE`, `MIN_CONFIDENCE`, RSI/MACD periyotları ve `CANDLE_*` değerlerini kendi verin için ayarla.
- Pazar kapandıktan sonra Gamma’dan sonuç gelince `data/trades.jsonl` içindeki ilgili satır güncellenir (`outcome`, `pnl`, `resolved_at`).

## Lisans ve sorumluluk

Bu yazılım yatırım tavsiyesi değildir. Canlı işlem öncesi riskleri kendin değerlendir; kayıplardan proje sorumlu tutulamaz.
