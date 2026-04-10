# Vendored `polymarket-client-sdk`

This is a copy of **crates.io `polymarket-client-sdk` 0.4.4** with a one-file patch to
`src/clob/order_builder.rs` (market `OrderBuilder::build` amount truncation).

**Reason:** The published SDK truncates market-order notionals to `tick_decimals + 2`, which can
exceed the CLOB API limits for FAK/FOK orders (`POST /order` returns 400 *invalid amounts*).
See [rs-clob-client#261](https://github.com/Polymarket/rs-clob-client/issues/261).

**Remove** this directory and the `[patch.crates-io]` entry in the workspace `Cargo.toml` when a
fixed release is published on crates.io.
