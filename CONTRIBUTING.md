# Contributing

```sh
cargo test                       # hermetic suite, a few seconds
AIRBRIDGE_ALLOW_NETWORK=1 cargo test --test hardware -- --ignored hardware   # real LAN and scanner
cargo bench --bench perf         # criterion baselines
scripts/perf-smoke.sh            # startup, idle RSS, idle CPU of the release binary
cd fuzz && cargo +nightly fuzz run scan_settings corpus/scan_settings seeds/scan_settings
```

- **Tests never touch the network.** `.cargo/config.toml` runs every test binary through `scripts/netns-runner.sh`, inside an empty network namespace. Set `AIRBRIDGE_ALLOW_NETWORK=1` for `cargo run` and the hardware suite.
- **Building needs `nasm`** for libjpeg-turbo's SIMD code.
- **CI** fails if the suite takes over 15 s, if a benchmark regresses more than 20% against the base commit, or if the image exceeds `container-size-budget`. It also fuzzes ScanSettings for 60 s.

Baselines measured on an Intel Core i7-12700T:

| Metric | airbridge | AirSane |
|---|---|---|
| JPEG, 600 dpi US Letter RGB | 116 ms | 128–131 ms (`scripts/airsane-jpeg-baseline.sh`) |
| ScannerCapabilities p99 | 0.12–0.28 ms | — |
| Startup to first announcement | 325 ms | — |
| Idle RSS / CPU | 6.3 MiB / ~0 | — |
