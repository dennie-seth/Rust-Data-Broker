# Changelog

## [0.6.0] - 2026-04-12

### Features

#### Custom arena allocator (`src/memory/allocator.rs`)
- New `#[global_allocator]` — bump allocator with first-fit free-list reuse, background defragmentation, and system allocator fallback pre-init. Replaces the standard allocator with a single pre-committed arena (`VirtualAlloc` on Windows, `mmap` on Unix) sized by the `MEMORY_LIMIT` config field.
- `AllocHeader` stored before each returned pointer allows `dealloc` to recover slot boundaries without relying on `Layout`.
- Free-list with block splitting: when a free block is larger than needed by at least `MIN_SLOT_SIZE`, the remainder is split off and returned to the free list.
- Background defragmentation thread (`arena-defrag`): triggered every 32nd dealloc via condvar, sorts the free list by address, and merges adjacent blocks.
- `LOCKED_SIZE` tail reservation: `reserve_used` enforces `used + needed <= capacity - LOCKED_SIZE` on every allocation, ensuring headroom for incoming messages.
- `set_locked_size()` with capacity guard — prevents setting a locked size that would exceed available space.
- `get_free_mem_size()` returns `usize::MAX` when the arena isn't initialized, so the `read_buffer` payload size guard doesn't reject requests in test builds.
- System allocator fallback for pre-init allocations and out-of-arena pointers in `dealloc`.

#### `MEMORY_LIMIT` config field (`src/config.rs`, `src/main.rs`)
- New `memory_limit: u64` field in `Config`, parsed from `MEMORY_LIMIT` key in `.settings`. Passed to `allocator::init()` on startup.

### Bug Fixes

#### `get_free_mem_size` returned 0 without arena (`src/memory/allocator.rs`)
- When the arena wasn't initialized (e.g. in tests), `get_free_mem_size()` returned `0` (`allocated - used = 0 - 0`). This made `read_buffer`'s size guard `(payload_size * SIZE_FACTOR) >= get_free_mem_size()` always true, rejecting every payload. Fixed by returning `usize::MAX` when `allocated == 0`.

#### `find_biggest_payloads` shift logic (`src/net/queue.rs`)
- The inner shift loop always shifted ALL elements `[0..4]` left regardless of the insertion point (`decr_id`). When `decr_id < 4` this lost elements above the insertion point and created duplicates. Fixed to only shift `[0..decr_id]`.

#### Stale `LOCKED_SIZE` TODO removed (`src/net/server.rs`)
- The `TODO(bug)` about `server.rs` writing `LOCKED_SIZE` directly via `.store()` was outdated — the code already uses `set_locked_size()` for all writes. TODO removed.

### Tests
- Added `memory_limit: 0` to `make_config` in both `server_tests.rs` and `load_tests.rs` to match the new `Config` field.
- All 34 unit tests pass; all 5 load tests pass (including 1 GB round-trip).

### Documentation

#### `CLAUDE.md`
- Added `MEMORY_LIMIT` to Config Fields table.
- Added `MemAllocator` to Key Types table.
- Known Design Limitations: removed fixed `LOCKED_SIZE` bypass and `find_biggest_payloads` bug. Added arena bump-pointer exhaustion under churn.

## [0.5.3] - 2026-04-11

### Bug Fixes

#### NetStats payload framing (`src/net/net_stats.rs`)
- `StatMessage::to_bytes` previously never wrote `self.queue_name` at all — it wrote the constant `size_of::<u64>() = 8` as 8 BE bytes where the name should have gone, followed by four raw `usize` fields with no delimiter. Clients had no way to parse the payload. Now writes `[2 BE u16 name_len][name bytes]` before the four stat fields.
- `StatWatcher::to_bytes` previously concatenated `StatMessage` byte streams with no count header or per-entry framing. Now prefixes the payload with `[4 BE u32 count]` and each entry with `[4 BE u32 entry_len]`, making the full `NetStats` response decodable.
- Remaining known issue: the four numeric stat fields are still serialized as `usize`, which is 4 bytes on 32-bit targets and 8 bytes on 64-bit — a cross-target client cannot parse them reliably. Tracked in Known Design Limitations.

### Tests

#### `server_net_stats_responds_success` (`src/tests/server_tests.rs`)
- Upgraded from a status-byte smoke test to a full payload decoder: reads the `u32` count header, the per-entry `u32` length prefix, the `u16` length-prefixed queue name, and the four `usize` stat fields, and asserts `count == 1`, `name == "statq"`, `total_messages == 1`, `total_messages_locked == 0`.

### Documentation

#### `CLAUDE.md`
- `NetStats` row in the command table rewritten to document the new payload format (`[count][entry_len][name_len][name][stats]`).
- Known Design Limitations: removed four now-fixed items (`QueueMessage::deep_size_of` × 32 inflation, `Queue::get_total_messages` / `get_total_messages_locked` returning HashMap capacity, `lock_to_read` dead `None` arm, full `NetStats` unparseable framing). Added two new entries: the residual platform-dependent `usize` in `NetStats` stat fields, and the `Queue::enqueue` tombstone-derived ID collision (when the trailing slot of `self.order` is `0`, the next id resolves to `1` and overwrites an existing `id=1` message).

### Internal
- Stale `TODO(bug)` comments removed from `src/net/queue.rs` for fixes that had already landed (`QueueMessage::deep_size_of`, `lock_to_read` iterator shape, `Queue::get_total_messages`, `Queue::get_total_messages_locked`).
- New `TODO(bug)` added in `Queue::enqueue` describing the tombstone ID-collision bug.
- `StatMessage::to_bytes` TODO replaced with a narrower note about the platform-dependent `usize` stat fields; `StatWatcher::to_bytes` TODO removed (framing now has count + per-entry length prefix).

## [0.5.0] - 2026-04-06

### Breaking Changes

#### Renamed `auto_success` → `auto_fail` (`src/net/queue.rs`, `src/net/server.rs`)
- `QueueConfig` fields renamed: `auto_success` → `auto_fail`, `success_timeout` → `fail_timeout`.
- `NetQueueConfig` accessors renamed: `auto_success()` → `auto_fail()`, `success_timeout()` → `fail_timeout()`.
- `Queue` accessors renamed: `get_config_auto_success` → `get_config_auto_fail`, `get_config_success_timeout` → `get_config_fail_timeout`, `update_config_auto_success` → `update_config_auto_fail`, `update_config_success_timeout` → `update_config_fail_timeout`.
- Wire format is unchanged — only Rust-side names were updated. The rename reflects the actual behavior: the feature NACKs (unlocks) messages after a timeout, it does not ACK them.

#### `Queue::unlock` signature changed (`src/net/queue.rs`)
- `unlock(&mut self, client_id: u128)` → `unlock(&mut self, client_id: u128, message_id: Option<u128>)`. When `message_id` is `Some`, unlock targets that specific message in the client's lock vec; when `None`, it pops the first lock (preserving the old behavior for `Request::Failed`).

### Features

#### `Queue::is_locked` (`src/net/queue.rs`)
- New `is_locked(&self, client_id: u128, message_id: u128) -> bool` method — checks whether a specific message is currently locked by a given client. Used by the auto-fail guard in `lock_and_dequeue_message`.

### Bug Fixes

#### Auto-fail rewrite (`src/net/server.rs`)
- `lock_and_dequeue_message` no longer calls `dequeue_message_sent` (ACK) after the timeout — it now calls `Queue::unlock` (NACK), matching the intended "visibility timeout" semantics: if the client doesn't ack in time, the message goes back on the queue for redelivery.
- Changed sleep unit from `Duration::from_secs` to `Duration::from_millis` for finer-grained control.
- Added early return when `fail_timeout == 0` (auto-fail enabled but no timeout → skip the sleep entirely).
- Added `is_locked` guard before `unlock`, held under the same `queue.lock().await` acquisition, preventing both a panic (`.position(...).unwrap()` on a missing entry) and stale NACKs when the client races in a Succeeded ack during the sleep.

#### `Queue::unlock` targeted unlock (`src/net/queue.rs`)
- When `message_id` is `Some` and present in the client's lock vec, `unlock` now finds and removes that specific entry by position instead of always popping index 0. This prevents auto-fail from unlocking the wrong message when a client holds multiple locks.
- When `message_id` is `Some` but not in the lock vec (e.g. already acked), the fallback to `first()` prevents a panic in the position lookup.

#### `Request::Succeeded` handler (`src/net/server.rs`)
- Refactored to call `dequeue_message_sent` (the existing helper) instead of inlining the `get_queue` + `dequeue` calls. No behavioral change — deduplicates the ACK path.

#### `Request::Failed` handler (`src/net/server.rs`)
- Updated to pass `None` as `message_id` to the new `unlock` signature, preserving "unlock first lock" semantics.

### Documentation
- `CLAUDE.md` command table: `auto_success` → `auto_fail`, `success_timeout` → `fail_timeout` in UpdateQ wire format.
- `CLAUDE.md` Known Design Limitations: rewrote `auto_fail` entry — documents the NACK-on-timeout behavior, millisecond units, and the `is_locked` guard that prevents the ack-race panic.

## [0.4.3] - 2026-04-05

### Features

#### UpdateQ command (`src/net/server.rs`, `src/net/queue.rs`)
- New `UpdateQ` request (command byte `11`) — updates per-queue configuration at runtime. Payload is a `NetQueueConfig` wire format: `[1 byte flags][auto_success: u8 if flag 0x01][success_timeout: u64 BE if flag 0x02]` (flags are independent, either field may be omitted).
- Added `QueueConfig` (storage) and `NetQueueConfig` (wire) types in `queue.rs`, plus `Queue::{get_config_auto_success, get_config_success_timeout, update_config_auto_success, update_config_success_timeout}` accessors.
- `lock_and_dequeue_message` (renamed from `send_message`) honors `auto_success`: after sending the dequeue response, it sleeps for `success_timeout` seconds and auto-acks the lock via `dequeue_message_sent`.

#### Multi-lock per client (`src/net/queue.rs`)
- `Queue.locked` widened from `HashMap<u128, u128>` to `HashMap<u128, Arc<RwLock<Vec<u128>>>>` so a single client can hold locks on multiple messages simultaneously. `lock_to_read`, `dequeue`, `unlock`, and `requeue` became `async`.

### Bug Fixes

#### Queue (`src/net/queue.rs`)
- `dequeue` no longer deadlocks on the Succeeded/no-id path — an `RwLockReadGuard` temporary from `ids.read().await.clone().into_iter().next()` was living through the subsequent `.write().await` on the same `RwLock`, causing the writer to wait forever. Replaced with a scoped `ids.read().await.first().copied()` so the read guard drops before the write is taken. This was the root cause of `server_succeeded_acks_message` and `server_delete_other_message_preserves_held_lock` hanging.
- `dequeue` by-ID path no longer panics when the caller never locked anything — the unconditional `self.locked.get(&client_id).unwrap()` was reachable because any client can dequeue any message by ID. Now guarded by `contains_key`.
- `dequeue` by-ID path no longer drops unrelated locks when acking via explicit `message_id` — previously it called `self.locked.remove(&client_id)` which wiped the client's entire lock list. Now it finds the entry matching `message_id` in the inner `Vec` and removes only that one.
- `dequeue` first-locked path (Succeeded/no-id) no longer drops unrelated locks — previously it called `self.locked.remove(&client_id)` after acking the first message; now it pops just index 0 from the inner `Vec`.
- `unlock` no longer deadlocks on the multi-lock `RwLock` — prior code held a read guard from `ids.read().await.first()` across `ids.write().await.remove(0)`. Now the first id is cloned out first, read guard drops, then write is taken.

#### Server (`src/net/server.rs`)
- Unknown command byte no longer propagates `?` (which panicked upstream) — the handler now sends a `Failed` response and returns `Err(BrokenPipe)` to close the connection cleanly.
- `read_buffer`'s `tokio::select!` block was re-indented so `stop_word.notified()` and `read_buf` are actual arms of the same select rather than sequential statements.
- `send_message` renamed to `lock_and_dequeue_message`, `clear_message_sent` renamed to `dequeue_message_sent` to reflect what they actually do.
- Protocol size constants (`COMMAND_SIZE`, `PAYLOAD_SIZE`, `CLIENT_ID_SIZE`, `QUEUE_NAME_SIZE`) promoted from `static` to `const`.

#### Config (`src/config.rs`)
- `QUEUE_NAMES` parsing now filters empty segments (`.filter(|s| !s.is_empty())`) so a trailing comma, leading comma, or empty `QUEUE_NAMES=` line no longer pre-creates a queue named `""`.

### Tests

#### New unit-level tests (`src/tests/server_tests.rs`)
- `server_update_queue_nonexistent_responds_failure` — UpdateQ against a missing queue returns Failed.
- `server_update_queue_valid_config_responds_success` — UpdateQ with a valid `NetQueueConfig` payload (flags=0b11, auto_success=true, success_timeout=5) returns Succeeded.
- `server_update_queue_empty_payload_responds_failure` — UpdateQ with an empty payload returns Failed (guards `payload_size < 1`).
- `server_drain_fires_when_accept_stops` replaces the previously `#[ignore]`d `server_graceful_shutdown_drains_connections`. The old test tried to deliver a Windows `CTRL_C_EVENT` to Tokio's signal handler after calling `SetConsoleCtrlHandler(NULL, TRUE)`, which per MSDN tells the OS to *ignore* Ctrl+C entirely — the event was suppressed before Tokio saw it, so `shutdown()` was never invoked and the test timed out at 30s. The replacement directly notifies `stop_accepting` and asserts `drained` fires within 5s, which is the actual behavior the test cared about.

#### New load/stress tests (`src/tests/load_tests.rs`, all `#[ignore]`; run with `cargo test --release -- --ignored --nocapture`)
- `pipelined_producer_does_not_stall` — one client writes 1000 enqueue requests back-to-back in a single `write_all`, then drains 1000 responses. Regression for the `read_buffer` while-loop drain fix.
- `n_producers_m_consumers_each_message_delivered_once` — 4 producers × 4 consumers × 500 msgs on a shared queue. Asserts every unique payload is delivered exactly once (no loss, no duplicates, no spurious payloads).
- `large_payload_roundtrip_10mb` — enqueue + dequeue of a 10 MB deterministic payload with byte-for-byte verification. Exercises `BytesMut` growth past its 4 KB initial capacity.
- `large_payload_roundtrip_100mb` — same shape, 100 MB. Verifies `BytesMut` can hold a ~100 MB request frame and the dequeue response (56-byte meta + 100 MB payload) is written/read intact.

### Documentation
- `CLAUDE.md` command table: added `UpdateQ` row (command 11) with `NetQueueConfig` wire format.
- `CLAUDE.md` Commands section: added `cargo test --release -- --ignored --nocapture` recipe for load tests.
- `CLAUDE.md` Tests section: added table describing `server_tests.rs` vs `load_tests.rs` purposes.
- `CLAUDE.md` Known Design Limitations: added unbounded `payload_size` DoS surface, added unconditional `auto_success` auto-ack (no ack-vs-timeout race).

## [0.4.2] - 2026-04-05

### Bug Fixes

#### Server (`src/net/server.rs`)
- `read_buffer` now drains every complete message from the read buffer per wakeup (was parsing at most one per `read_buf` call, so pipelined requests could deadlock waiting for bytes that had already arrived)
- `read_buffer` uses `break` (not `continue`) when the payload hasn't fully arrived, so control returns to the outer `read_buf` select instead of infinite-looping over the same header
- `DeleteM` and `Requeue` no longer panic when the payload is longer than 16 bytes — switched from `as_slice().try_into()` (requires exact length) to `payload[..16].try_into()` (combined with the existing `< 16` guard)
- `DeleteQ` now removes the name→hash entry from `queue_names` in addition to the queue itself; previously the orphan mapping persisted, causing later operations on the name to return confusing "Queue not found" errors and blocking clean reuse of the name

#### Queue (`src/net/queue.rs`)
- `lock_to_read` no longer panics when `next_id` points at a message not present in `self.queue` — added a `contains_key` guard that returns `Err("No such message id")` instead of indexing
- Replaced the unsound `binary_search` on `self.order` (which may contain `0` holes and, after `u128` wrap, is not sorted) with a direct membership/guard check
- `lock_to_read` and `dequeue` now advance `next_id` past `0` holes via `self.order.iter().find(|&&x| x != 0).copied()` instead of falling back to `Some(1)` (which was not guaranteed to correspond to any live message)
- `dequeue(_, Some(message_id))` no longer clears the caller's lock on an *unrelated* message — the `self.locked.remove(&client_id)` is now guarded by `self.locked.get(&client_id) == Some(&message_id)`
- `dequeue(_, Some(message_id))` now returns `Ok(())` on its explicit-ID path, fixing a fall-through that previously deleted a second unrelated message (the one the client held a lock on) and returned a spurious `Failed` for lock-less callers

### Tests
- Added `server_delete_other_message_preserves_held_lock` — regression test that locks one message, then calls `DeleteM` on a *different* message, then acks the original lock; verifies the held lock is preserved and the acked message was not collaterally deleted
- Added `src/tests/load_tests.rs` (all `#[ignore]`; run with `cargo test --release -- --ignored --nocapture`):
  - `pipelined_producer_does_not_stall` — one client writes 1000 enqueue requests back-to-back in a single `write_all`, then drains 1000 responses. Regression for the `read_buffer` while-loop drain fix (would have deadlocked before).
  - `n_producers_m_consumers_each_message_delivered_once` — 4 producers × 4 consumers × 500 msgs on a shared queue. Asserts every unique payload is received exactly once (no loss, no duplicates, no spurious payloads). Catches next_id advancement, lock/ack races, and duplicate-delivery bugs.
  - `large_payload_roundtrip_10mb` — enqueue + dequeue of a single 10 MB deterministic payload with byte-for-byte verification. Exercises `BytesMut` growth (initial capacity is only 4 KB) and the large-response write path.

### Documentation
- `CLAUDE.md` command table: `PeekM` renamed to `ListM` to match the code (`Request::ListM = 5`)
- `CLAUDE.md` Known Design Limitations: removed the `locked_by` / `0xFFFF` sentinel entry (already using `u128::MAX`); added the `u128` ID-wrap collision note as a documented intentional limitation

## [0.3.5] - 2026-04-04

### Features
- Implemented `UpdateM` command — updates a message's payload by ID (payload = 16-byte message ID + new payload)
- Implemented `Requeue` command handler in server (queue-side `requeue` already existed)

### Bug Fixes
- Fixed `UpdateM` payload size check: was `!= 16` (rejecting any update with actual data), now `< 16`
- Fixed `DeleteM` and `Requeue` payload size checks: moved from `!= 16` to `< 16` for consistency

### Tests
- Added `server_update_message_changes_payload` — enqueues, updates payload via `UpdateM`, then verifies the new payload on re-dequeue
- Added `server_update_message_too_short_payload_fails` — verifies `UpdateM` with a payload shorter than 16 bytes returns Failed

### Documentation
- `CLAUDE.md` updated: `Requeue` and `UpdateM` commands documented as implemented; removed from Known Design Limitations

## [0.3.4] - 2026-04-02

### Bug Fixes

#### Tests
- Fixed `server_enqueue_then_dequeue_returns_message` — payload assertion offset adjusted from `response[9..]` to `response[9 + 56..]` to account for the 56-byte meta prefix added to dequeue responses in 0.3.4

### Tests
- Added `server_delete_queue_responds_success` — verifies `DeleteQ` on an existing queue returns Succeeded
- Added `server_delete_queue_nonexistent_responds_failure` — verifies `DeleteQ` on a missing queue returns Failed
- Added `server_list_messages_returns_metadata` — verifies `ListM` returns the correct number of 56-byte `Meta` entries
- Added `server_succeeded_acks_message` — verifies `Succeeded` removes the locked message and the queue is empty afterwards
- Added `server_failed_nacks_and_unlocks_message` — verifies `Failed` unlocks the message so it can be dequeued again
- Added `server_delete_message_by_id` — verifies `DeleteM` removes a specific message by its 16-byte ID
- Added `server_requeue_moves_message_to_end` — verifies `Requeue` moves a message to the back of the queue

## [0.3.3] - 2026-03-29

### Bug Fixes

#### Protocol
- `client_id` field widened from `u16` (2 bytes) to `u128` (16 bytes) to match the wire protocol's `CLIENT_ID_SIZE = 16`; the previous type caused a guaranteed panic on `try_into().unwrap()` at runtime
- `RequestMessage::new` was reading payload from the wrong byte offsets (old 9-byte header); updated to use the full header size (`command + client_id + payload_size + queue_name`)
- Queue name is now stripped of null-byte padding (`trim_end_matches('\0')`) after parsing the fixed 64-byte wire field; previously all `HashMap` lookups silently failed because the padded key never matched any stored entry

#### Queue
- `publisher_id` and `locked_by` fields widened from `u16` to `u128` throughout `QueueMessage`, `Meta`, and `Queue`
- `PartialEq<u16> for QueueMessage` replaced with `PartialEq<u128>`; fixed a panic — `locked_by.unwrap()` was called unconditionally, now uses `map_or`
- `Meta::to_be_bytes` fixed panic on unlocked messages — `locked_by.unwrap()` replaced with `map_or(u128::MAX, ...)`
- `Queue::new()` initialised `next_id` to `Some(0)`, but message IDs start at 1; changed to `None`
- `enqueue` now sets `next_id = Some(id)` when the queue was previously empty, fixing `lock_to_read` returning "Queue is empty" on a non-empty queue
- `enqueue` now starts IDs at 1 (not 0) to avoid collision with the `0` deletion sentinel used by `dequeue` / `remove_zeroes`; overflow wrapping to 0 is also skipped
- `unlock` now resets `next_id = Some(id)` when `next_id` is `None` (queue fully consumed), so an unlocked message becomes dequeue-able again

#### Server
- `Server.queue` changed from a plain `HashMap` (deep-cloned into each `Server` clone) to `Arc<RwLock<HashMap<...>>>`, so `CreateQ` and `DeleteQ` mutations are visible to all tasks immediately
- All `queue` accesses updated to `read().await` / `write().await` accordingly

### Removed
- `BrokerClient` struct — was never constructed or used anywhere

### Tests
- Rewrote all server integration tests to use the correct binary protocol (`client_id` + `queue_name` fields were missing from all requests)
- Added `encode_request(command, client_id, queue_name, payload)` helper that builds a correctly-framed request
- Added `read_response` helper that reads the variable-length response header + payload
- Added `server_create_queue_responds_success` — verifies `CreateQ` returns Succeeded
- Added `server_create_queue_duplicate_responds_failure` — verifies duplicate `CreateQ` returns Failed
- Added `server_graceful_shutdown_drains_connections` — verifies the `drained` notify fires after `stop_accepting` + `stop_word` are signalled
- Tests now use `CreateQ` to set up queues instead of relying on pre-configured `queue_names`, keeping tests self-contained and avoiding the null-padding mismatch with config-supplied names

### Documentation
- `CLAUDE.md` protocol section updated: all 10 commands documented with descriptions; request frame updated to include `client_id` and `queue_name` fields
- Key Types table updated: removed stale `BrokerClient` entry, added `Queue`
- Config table updated: added `QUEUE_NAMES` field
- Known Design Limitations updated to reflect current state