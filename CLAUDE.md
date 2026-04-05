# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

DataBroker is a tutorial Rust TCP-based message queue. Clients connect and issue commands over a binary protocol. Named queues are shared across all connected clients. It's explicitly a learning project — the codebase contains many TODO comments documenting known bugs and design issues.

## Commands

```bash
# Build
cargo build
cargo build --release

# Run (debug)
cargo run

# Test
cargo test

# Run a single test
cargo test <test_name>

# Load tests (ignored by default; must be run in release mode)
cargo test --release -- --ignored --nocapture
```

**Configuration:** Pass `--config=<path>` to specify the settings file. Without the flag, it looks for `.settings` in the current working directory.

**Graceful shutdown (Linux only):** Pass `--shutdown_timeout=<secs>` to enable drain-and-wait on SIGTERM. Value `0` is treated as 30 seconds. Without the flag, SIGTERM stops immediately (same as Ctrl+C).

## Architecture

### Threading Model

- **Main thread**: Tokio multi-threaded runtime; handles graceful shutdown (Ctrl+C / SIGTERM). Two notifies control shutdown: `stop_accepting` (stop taking new connections) and `stop_word` (stop all active connections). A third notify, `drained`, is fired by `Server::run` once `active_connections` reaches zero.
- **Worker pool** (`Pool` in `src/net/server.rs`): N OS threads, each running a single-threaded Tokio runtime; jobs are dispatched via `std::sync::mpsc::sync_channel` to the least-loaded worker (pressure-based, tracked with `AtomicUsize` per worker)
- **Per-connection tasks**: Spawned via `tokio::spawn` inside worker threads

### Message Protocol (binary, big-endian)

**Request:** `[1 byte command][16 bytes client_id][8 bytes payload_size][64 bytes queue_name][payload]`

| Command | Value | Description |
|---------|-------|-------------|
| Enqueue | 1 | Push a message onto a named queue |
| Dequeue | 2 | Lock and read the next message from a named queue |
| CreateQ | 3 | Create a new named queue at runtime |
| DeleteQ | 4 | Delete a named queue |
| ListM   | 5 | List message metadata for a named queue |
| DeleteM | 6 | Delete a specific message by ID (payload = 16-byte message ID) |
| Succeeded | 7 | Acknowledge processing — dequeues the message locked by this client |
| Failed  | 8 | Nack — unlocks the message so another client can dequeue it |
| Requeue | 9 | Move a message to the end of the queue (payload = 16-byte message ID) |
| UpdateM | 10 | Update a message's payload (payload = 16-byte message ID + new payload) |
| UpdateQ | 11 | Update per-queue config (payload = `NetQueueConfig`: 1 byte flags + optional `auto_success: u8` + optional `success_timeout: u64 BE`) |

**Response:** `[1 byte status][8 bytes payload_size][payload]`
- Status `1` = Succeeded, `2` = Failed

### Key Types

| Type | File | Purpose |
|------|------|---------|
| `Config` | `src/config.rs` | Parsed from `.settings` (key=value, regex-based) |
| `Server` | `src/net/server.rs` | TCP listener; accepts connections and hands to `Pool` |
| `Pool` | `src/net/server.rs` | Worker thread pool with pressure-based dispatch |
| `Queue` | `src/net/queue.rs` | Named message queue with lock-to-read / ack semantics |

### Tests

| File | Purpose |
|------|---------|
| `src/tests/server_tests.rs` | Unit-level integration tests — one test per command / edge case. Run by default with `cargo test`. |
| `src/tests/load_tests.rs` | `#[ignore]`-gated concurrency and throughput tests. Run with `cargo test --release -- --ignored --nocapture`. Each test starts its own server on a free local port. |

### Config Fields

| Key | Field | Notes |
|-----|-------|-------|
| `THREADS_LIMIT` | `threads_limit` | Worker thread count |
| `WAIT_LIMIT` | `wait_limit` | Channel capacity per worker |
| `SERVER_ADDR` | `server_addr` | Listen IP |
| `SERVER_PORT` | `server_port` | Listen port |
| `PROC_LIMIT` | `proc_limit` | Parsed but currently unused |
| `QUEUE_NAMES` | `queue_names` | Comma-separated list of queues pre-created at startup |

### Known Design Limitations (intentional, for learning)

- `command` field in `RequestMessage` is stored but never read after construction
- `PROC_LIMIT` is parsed but never used
- On `u128` ID overflow, `enqueue` wraps the next ID back to 1, which may collide with an older message still in `self.queue` and silently overwrite it (see `TODO(note)` in `src/net/queue.rs`)
- `read_buffer` has no upper bound on `payload_size`, so a client can request an arbitrarily large `BytesMut` allocation (DoS surface)
- Per-queue `auto_success` auto-acks a locked message after `success_timeout` seconds unconditionally — there is no "ack-or-auto-ack-on-timeout" race, the auto-ack always fires