# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

DataBroker is a tutorial Rust TCP-based message queue. Clients connect and issue Enqueue/Dequeue commands over a binary protocol. It's explicitly a learning project — the codebase contains many TODO comments documenting known bugs and design issues.

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
```

**Configuration:** Pass `--config=<path>` to specify the settings file. Without the flag, it looks for `.settings` in the current working directory.

## Architecture

### Threading Model

- **Main thread**: Tokio multi-threaded runtime; handles graceful shutdown (Ctrl+C / SIGTERM)
- **Worker pool** (`Pool` in `src/net/server.rs`): N OS threads, each running a single-threaded Tokio runtime; jobs are dispatched via `std::sync::mpsc::sync_channel` to the least-loaded worker (pressure-based, tracked with `AtomicUsize` per worker)
- **Per-connection tasks**: Spawned via `tokio::spawn` inside worker threads

### Message Protocol (binary, big-endian)

**Request:** `[1 byte command][8 bytes payload_size][payload]`
- Command `1` = Enqueue, `2` = Dequeue

**Response:** `[1 byte status][8 bytes payload_size][payload]`
- Status `1` = Succeeded, `2` = Failed

### Key Types

| Type | File | Purpose |
|------|------|---------|
| `Config` | `src/config.rs` | Parsed from `.settings` (key=value, regex-based) |
| `Server` | `src/net/server.rs` | TCP listener; accepts connections and hands to `Pool` |
| `Pool` | `src/net/server.rs` | Worker thread pool with pressure-based dispatch |
| `BrokerClient` | `src/net/server.rs` | Per-connection `VecDeque` queue (not shared across clients) |

### Config Fields

| Key | Field | Notes |
|-----|-------|-------|
| `THREADS_LIMIT` | `threads_limit` | Worker thread count |
| `WAIT_LIMIT` | `wait_limit` | Channel capacity per worker |
| `SERVER_ADDR` | `server_addr` | Listen IP |
| `SERVER_PORT` | `server_port` | Listen port |
| `PROC_LIMIT` | `proc_limit` | Parsed but currently unused |

### Known Design Limitations (intentional, for learning)

- Each connection has an **isolated queue** — no inter-client message sharing (not a true broker)
- `clients` map is write-only — entries are removed on disconnect but nothing reads the map to route messages between clients
- `command` field in `RequestMessage` is stored but never read after construction