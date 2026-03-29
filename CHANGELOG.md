# Changelog

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