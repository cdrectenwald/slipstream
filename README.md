# Slipstream

Slipstream is a small Rust limit-order-book and matching-engine project built to make exchange-core behavior easy to inspect, test, and discuss. It exists as a compact trading-infrastructure portfolio piece: the core matcher is deterministic and replayable, while the TCP and browser surfaces show how the same engine can sit behind operational interfaces without pulling networking concerns into the hot path.

## Architecture

```text
        CLI / TCP clients             Browser console
              |                             |
              v                             v
       plain-text protocol          tiny local HTTP API
              |                             |
              +-------------+---------------+
                            |
                            v
                   Command parser
                            |
                            v
                  Matching Engine
              price-time priority book
                            |
              +-------------+-------------+
              |                           |
              v                           v
        Append-only event log       Book snapshots / fills
              |
              v
        Recovery and replay
```

The engine lives in `src/lib.rs` and owns order validation, price-time priority, matching, cancellation, snapshots, and event-log replay. `src/main.rs` keeps the runnable surfaces thin: a demo command, a TCP primary, a submit client, a recovery command, and a local browser console.

## Quickstart

```sh
git clone <repo-url>
cd slipstream
cargo test
cargo run -- demo
```

To run the local browser console:

```sh
cargo run -- web 127.0.0.1:8080 target/slipstream-web.events
```

Then open `http://127.0.0.1:8080`.

## Tech Stack

- Rust 2024 edition
- Standard-library networking with `TcpListener` and `TcpStream`
- Append-only text event log for persistence and recovery
- `proptest` for randomized replay invariants
- GitHub Actions for build, test, formatting, and Clippy checks

## Commands

Run all local checks:

```sh
cargo fmt --check
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

Start a TCP primary:

```sh
cargo run -- serve 127.0.0.1:7000 target/slipstream-primary.events
```

Submit commands from another terminal:

```sh
cargo run -- submit 127.0.0.1:7000 LIMIT ASK 1 101 10
cargo run -- submit 127.0.0.1:7000 LIMIT BID 2 101 4
cargo run -- submit 127.0.0.1:7000 SNAPSHOT
```

Recover a follower-style copy from the event log:

```sh
cargo run -- recover target/slipstream-primary.events
```

The TCP protocol is intentionally plain text:

- `LIMIT <BID|ASK> <id> <price> <qty>`
- `CANCEL <id>`
- `SNAPSHOT`
- `QUIT`

## Design Decisions

### Deterministic Core Before Distribution

The matching engine is single-threaded and deterministic. Networking and HTTP handlers hold the engine behind a mutex, but the core API stays synchronous so replay, property tests, and order-priority reasoning remain straightforward.

### Append-Only Log Over Snapshot Persistence

Accepted events are appended to a simple text log and replayed during recovery. This makes crash recovery auditable and easy to test, at the cost of slower startup as logs grow. That tradeoff is deliberate for a small systems project where correctness and explainability matter more than production compaction.

### Price-Time Priority With Standard Collections

The book uses `BTreeMap` price levels and `VecDeque` FIFO queues. This keeps best-price lookup and same-price priority clear without introducing custom data structures before benchmarks prove they are needed.

## License

MIT
