# PrimeLedger

*Infinite sequence, no rewrites, mathematically pure.*

A self-contained, local-first, append-only double-entry ledger in Rust. No database server, no cloud,
no network calls. Planned to ship as a Microsoft Store (.appx/MSIX) app.

## Status

| Step | Crate | State |
|---|---|---|
| 1. Record format and hash-chain spec | `SPEC.md` | written |
| 2. Engine (entries, accounts, invariants, reversals, Merkle day roots) | `prime-core` | written, unit-tested |
| 3. Append-only file, fsync, crash recovery | `prime-store` | written, unit-tested |
| 4. Independent verifier and CLI | `prime-verify`, `prime-cli` | written, unit-tested |
| 5. Snapshots, bitemporal queries, FX helpers | `prime-core`, `prime-store`, `prime-cli` | written, unit-tested (v0.2) |
| 6. Pending/posted two-phase, rules engine, outbox, reconciliation | - | later |
| 7. `egui` app, MSIX packaging, signed roots (ed25519) | `prime-app` | later |

## Build and test

    cargo test --workspace
    cargo build --release -p prime-cli

CI (`.github/workflows/ci.yml`) runs build, tests and clippy on Linux and Windows, builds
`prime.exe`, and runs `cargo audit` (non-blocking). The first run produces `Cargo.lock` as an
artifact: commit it, then add `--locked` to the cargo commands.

## Try it

    prime open-account my.pld equity GBP
    prime open-account my.pld cash GBP --floor 0
    prime post my.pld seed-1 "opening capital" equity=-10000 cash=10000
    prime balances my.pld
    prime verify my.pld
    prime roots my.pld
    prime proof my.pld 3
    prime reverse my.pld 3 undo-seed-1
    prime balances my.pld --recorded-at 2026-03-03      # what we believed on 3 March
    prime balances my.pld --effective-at 2026-02-01     # what was true on 1 February
    prime fx my.pld fx-1 "GBP to USD" alice fx:gbp fx:usd bob 1000 1270
    prime snapshot my.pld                               # speed-up cache, not a source of truth

## Known limits (v0.1)

- Single segment file, still read fully on open (snapshots only skip re-validating old records). Segment rollover is later.
- Snapshots (`<file>.snap`) are a cache: used only if their head hash matches the log, ignored by `verify` and by historical queries.
- Single writer is by convention; there is no cross-process file lock yet.
- The verifier reuses `prime-core` for the chain rules, then independently re-sums every balance.
- An attacker with write access to the file can rewrite the *whole* chain consistently, or cut off
  the newest records. Defence: publish the head hash / `prime roots` output somewhere outside the
  machine. Device-key signatures (step 7) will bind records to the app.
- No GUI yet; no Store packaging yet.
