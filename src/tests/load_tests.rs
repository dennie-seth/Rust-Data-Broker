#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::sync::Mutex;
    use tokio::time::timeout;
    use crate::config::Config;
    use crate::net::server::{start_server, Notify};

    // --- shared helpers (kept local; server_tests' helpers live in a private module) ---

    fn free_local_addr() -> SocketAddrV4 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, address.port())
    }

    fn make_config(address: SocketAddrV4, threads: u64, wait: u64) -> Config {
        Config {
            threads_limit: threads,
            proc_limit: 1,
            wait_limit: wait,
            server_addr: address.ip().to_string(),
            server_port: address.port().to_string(),
            queue_names: vec![],
        }
    }

    fn encode_request(command: u8, client_id: u128, queue_name: &str, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + 16 + 8 + 64 + payload.len());
        buf.push(command);
        buf.extend_from_slice(&client_id.to_be_bytes());
        buf.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        let mut qname_buf = [0u8; 64];
        let name_bytes = queue_name.as_bytes();
        qname_buf[..name_bytes.len().min(64)].copy_from_slice(&name_bytes[..name_bytes.len().min(64)]);
        buf.extend_from_slice(&qname_buf);
        buf.extend_from_slice(payload);
        buf
    }

    /// Reads one response frame: [1 byte status][8 bytes payload_size][payload].
    /// Returns (status, payload).
    async fn read_response(client: &mut TcpStream, read_timeout: Duration) -> (u8, Vec<u8>) {
        let mut header = [0u8; 9];
        timeout(read_timeout, client.read_exact(&mut header))
            .await.expect("timed out reading response header").unwrap();
        let payload_size = u64::from_be_bytes(header[1..9].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; payload_size];
        if payload_size > 0 {
            timeout(read_timeout, client.read_exact(&mut payload))
                .await.expect("timed out reading response payload").unwrap();
        }
        (header[0], payload)
    }

    async fn create_queue(client: &mut TcpStream, client_id: u128, name: &str) {
        client.write_all(&encode_request(3, client_id, name, &[])).await.unwrap();
        let (status, _) = read_response(client, Duration::from_secs(5)).await;
        assert_eq!(status, 1, "CreateQ({name}): expected Succeeded");
    }

    async fn start_loaded_server(threads: u64, wait: u64) -> (SocketAddrV4, Arc<Notify>) {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address, threads, wait), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        (address, stop_word)
    }

    // ============================================================================
    // Test 3 — Pipelined producer
    //
    // One client writes K enqueue requests back-to-back WITHOUT waiting for any
    // response in between, then reads K responses. Before the read_buffer `while`
    // fix this would deadlock after the first message (server parsed one msg per
    // read_buf wakeup, leaving the rest stuck until more bytes arrived).
    //
    // Also serves as a rough throughput baseline for single-connection writes.
    // ============================================================================
    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pipelined_producer_does_not_stall() {
        const MSGS: usize = 1000;
        const PAYLOAD: &[u8] = b"pipelined-payload";

        let (address, stop_word) = start_loaded_server(2, 8).await;
        let mut client = TcpStream::connect(address).await.unwrap();
        create_queue(&mut client, 1, "pipeq").await;

        // Build one big buffer containing MSGS back-to-back enqueue requests.
        let mut batch = Vec::with_capacity((1 + 16 + 8 + 64 + PAYLOAD.len()) * MSGS);
        for _ in 0..MSGS {
            batch.extend_from_slice(&encode_request(1, 1, "pipeq", PAYLOAD));
        }

        let start = Instant::now();
        client.write_all(&batch).await.unwrap();

        // Drain MSGS responses. Generous timeout — if the server stalls, this
        // is where the deadlock would surface.
        for i in 0..MSGS {
            let (status, _) = read_response(&mut client, Duration::from_secs(10)).await;
            assert_eq!(status, 1, "pipelined enqueue #{i}: expected Succeeded");
        }
        let elapsed = start.elapsed();
        let rate = MSGS as f64 / elapsed.as_secs_f64();
        println!(
            "pipelined_producer_does_not_stall: {MSGS} msgs in {:?} ({:.0} msgs/sec)",
            elapsed, rate
        );

        stop_word.notify();
    }

    // ============================================================================
    // Test 4 — N producers × M consumers on one shared queue
    //
    // Each producer sends a disjoint set of unique payloads (encoded as big-endian
    // u64s) onto the same queue. Consumers run Dequeue → Succeeded in a loop until
    // the queue is empty. We assert:
    //   (a) every producer payload was received by some consumer
    //   (b) no payload was received twice
    //   (c) no unexpected payloads showed up
    //
    // This is the single highest-value correctness-under-load check — it catches
    // next_id advancement bugs, lock/ack races, duplicate delivery, and lost msgs.
    // ============================================================================
    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn n_producers_m_consumers_each_message_delivered_once() {
        const PRODUCERS: u64 = 4;
        const CONSUMERS: u64 = 4;
        const MSGS_PER_PRODUCER: u64 = 500;
        const TOTAL: u64 = PRODUCERS * MSGS_PER_PRODUCER;
        const QNAME: &str = "fanq";
        // 56 bytes meta prefix on a dequeue response: 16 id + 16 pub_id + 8 ts + 16 locked_by.
        const META_LEN: usize = 56;

        let (address, stop_word) = start_loaded_server(4, 32).await;

        // Setup queue via a dedicated connection.
        {
            let mut setup = TcpStream::connect(address).await.unwrap();
            create_queue(&mut setup, 0, QNAME).await;
        }

        // Producers: each sends MSGS_PER_PRODUCER u64 payloads with a disjoint prefix.
        // Payload encodes (producer_idx * MSGS_PER_PRODUCER + i) as 8 big-endian bytes.
        let mut producer_handles = Vec::new();
        for p in 0..PRODUCERS {
            producer_handles.push(tokio::spawn(async move {
                let mut client = TcpStream::connect(address).await.unwrap();
                for i in 0..MSGS_PER_PRODUCER {
                    let tag: u64 = p * MSGS_PER_PRODUCER + i;
                    let payload = tag.to_be_bytes();
                    client.write_all(&encode_request(1, p as u128 + 1, QNAME, &payload)).await.unwrap();
                    let (status, _) = read_response(&mut client, Duration::from_secs(5)).await;
                    assert_eq!(status, 1, "producer {p} msg {i}: expected Succeeded");
                }
            }));
        }

        // Consumers: Dequeue → Succeeded until they see EMPTY_STREAK_LIMIT failures in a row.
        let received: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::with_capacity(TOTAL as usize)));
        const EMPTY_STREAK_LIMIT: u32 = 50;

        let mut consumer_handles = Vec::new();
        for c in 0..CONSUMERS {
            let received = received.clone();
            consumer_handles.push(tokio::spawn(async move {
                let mut client = TcpStream::connect(address).await.unwrap();
                let client_id = (1000 + c) as u128;
                let mut empty_streak = 0u32;
                loop {
                    client.write_all(&encode_request(2, client_id, QNAME, &[])).await.unwrap();
                    let (status, payload) = read_response(&mut client, Duration::from_secs(5)).await;
                    if status != 1 {
                        empty_streak += 1;
                        if empty_streak >= EMPTY_STREAK_LIMIT {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        continue;
                    }
                    empty_streak = 0;
                    assert_eq!(payload.len(), META_LEN + 8, "consumer {c}: unexpected payload shape");
                    let tag = u64::from_be_bytes(payload[META_LEN..META_LEN + 8].try_into().unwrap());
                    received.lock().await.push(tag);

                    // Ack the lock.
                    client.write_all(&encode_request(7, client_id, QNAME, &[])).await.unwrap();
                    let (ack_status, _) = read_response(&mut client, Duration::from_secs(5)).await;
                    assert_eq!(ack_status, 1, "consumer {c}: Succeeded ack expected");
                }
            }));
        }

        for h in producer_handles { h.await.unwrap(); }
        for h in consumer_handles { h.await.unwrap(); }

        let received = received.lock().await.clone();
        let got: HashSet<u64> = received.iter().copied().collect();
        let expected: HashSet<u64> = (0..TOTAL).collect();

        assert_eq!(received.len() as u64, TOTAL, "lost or extra messages (received {} of {TOTAL})", received.len());
        assert_eq!(got.len() as u64, TOTAL, "duplicate delivery: {} unique vs {} total", got.len(), received.len());
        assert_eq!(got, expected, "payload set mismatch (lost or spurious payloads)");

        println!(
            "n_producers_m_consumers: {PRODUCERS}p × {CONSUMERS}c × {MSGS_PER_PRODUCER} = {TOTAL} msgs delivered once",
        );

        stop_word.notify();
    }

    // ============================================================================
    // 10 MB payload round-trip
    //
    // Enqueue a single 10 MB payload, dequeue it, verify byte-for-byte equality.
    // Exercises the buffer-growth path in read_buffer (initial capacity is only
    // 4 KB — the BytesMut grows on demand) and the large-response write path.
    // ============================================================================
    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn large_payload_roundtrip_10mb() {
        const SIZE: usize = 10 * 1024 * 1024; // 10 MB
        const META_LEN: usize = 56;
        const QNAME: &str = "bigq";

        let (address, stop_word) = start_loaded_server(2, 8).await;
        let mut client = TcpStream::connect(address).await.unwrap();
        create_queue(&mut client, 1, QNAME).await;

        // Deterministic pattern so we can check every byte without storing a copy elsewhere.
        let payload: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();

        let start = Instant::now();
        client.write_all(&encode_request(1, 1, QNAME, &payload)).await.unwrap();
        let (status, _) = read_response(&mut client, Duration::from_secs(30)).await;
        assert_eq!(status, 1, "Enqueue 10MB: expected Succeeded");
        let enq_elapsed = start.elapsed();

        let start = Instant::now();
        client.write_all(&encode_request(2, 1, QNAME, &[])).await.unwrap();
        let (status, response_payload) = read_response(&mut client, Duration::from_secs(30)).await;
        let deq_elapsed = start.elapsed();
        assert_eq!(status, 1, "Dequeue 10MB: expected Succeeded");
        assert_eq!(response_payload.len(), META_LEN + SIZE, "dequeue response size mismatch");

        let got = &response_payload[META_LEN..];
        assert_eq!(got.len(), SIZE, "payload length mismatch");
        // Full byte-for-byte compare — assert_eq! on 10 MB slices prints a useful
        // first-difference on failure.
        assert!(got == payload.as_slice(), "10MB payload byte mismatch on round-trip");

        let mbps_enq = (SIZE as f64 / (1024.0 * 1024.0)) / enq_elapsed.as_secs_f64();
        let mbps_deq = (SIZE as f64 / (1024.0 * 1024.0)) / deq_elapsed.as_secs_f64();
        println!(
            "large_payload_roundtrip_10mb: enqueue {:?} ({:.1} MB/s), dequeue {:?} ({:.1} MB/s)",
            enq_elapsed, mbps_enq, deq_elapsed, mbps_deq
        );

        stop_word.notify();
    }

    // ============================================================================
    // 100 MB payload round-trip (stress)
    //
    // Same shape as the 10 MB test but 10× larger. Verifies that BytesMut can
    // grow to hold a ~100 MB request frame and that the dequeue response
    // (56-byte meta + 100 MB payload) is written and read back intact. Expect
    // this one to take noticeably longer and to be memory-hungry on the
    // server side — peak RSS will be at least ~200 MB (request buffer +
    // stored message + outbound write buffer).
    // ============================================================================
    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn large_payload_roundtrip_100mb() {
        const SIZE: usize = 100 * 1024 * 1024; // 100 MB
        const META_LEN: usize = 56;
        const QNAME: &str = "hugeq";

        let (address, stop_word) = start_loaded_server(2, 8).await;
        let mut client = TcpStream::connect(address).await.unwrap();
        create_queue(&mut client, 1, QNAME).await;

        // Deterministic pattern so we can check every byte without storing a copy elsewhere.
        let payload: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();

        let start = Instant::now();
        client.write_all(&encode_request(1, 1, QNAME, &payload)).await.unwrap();
        let (status, _) = read_response(&mut client, Duration::from_secs(120)).await;
        assert_eq!(status, 1, "Enqueue 100MB: expected Succeeded");
        let enq_elapsed = start.elapsed();

        let start = Instant::now();
        client.write_all(&encode_request(2, 1, QNAME, &[])).await.unwrap();
        let (status, response_payload) = read_response(&mut client, Duration::from_secs(120)).await;
        let deq_elapsed = start.elapsed();
        assert_eq!(status, 1, "Dequeue 100MB: expected Succeeded");
        assert_eq!(response_payload.len(), META_LEN + SIZE, "dequeue response size mismatch");

        let got = &response_payload[META_LEN..];
        assert_eq!(got.len(), SIZE, "payload length mismatch");
        // Full byte-for-byte compare — use `==` on raw slices rather than
        // assert_eq! so a mismatch doesn't dump 100 MB into the test log.
        assert!(got == payload.as_slice(), "100MB payload byte mismatch on round-trip");

        let mbps_enq = (SIZE as f64 / (1024.0 * 1024.0)) / enq_elapsed.as_secs_f64();
        let mbps_deq = (SIZE as f64 / (1024.0 * 1024.0)) / deq_elapsed.as_secs_f64();
        println!(
            "large_payload_roundtrip_100mb: enqueue {:?} ({:.1} MB/s), dequeue {:?} ({:.1} MB/s)",
            enq_elapsed, mbps_enq, deq_elapsed, mbps_deq
        );

        stop_word.notify();
    }

    // ============================================================================
    // 1 GB payload round-trip (stress)
    //
    // Same shape as the 100 MB test but 10× larger. Verifies that the server
    // can handle a single ~1 GB request frame end-to-end: allocation, storage,
    // and faithful dequeue. Peak RSS will be at least ~2 GB (request buffer +
    // stored message + outbound write buffer). Timeouts are generous to
    // accommodate slower CI machines.
    // ============================================================================
    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn large_payload_roundtrip_1gb() {
        const SIZE: usize = 1024 * 1024 * 1024; // 1 GB
        const META_LEN: usize = 56;
        const QNAME: &str = "gbq";

        let (address, stop_word) = start_loaded_server(2, 8).await;
        let mut client = TcpStream::connect(address).await.unwrap();
        create_queue(&mut client, 1, QNAME).await;

        // Deterministic pattern so we can check every byte without storing a copy elsewhere.
        let payload: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();

        let start = Instant::now();
        client.write_all(&encode_request(1, 1, QNAME, &payload)).await.unwrap();
        let (status, _) = read_response(&mut client, Duration::from_secs(300)).await;
        assert_eq!(status, 1, "Enqueue 1GB: expected Succeeded");
        let enq_elapsed = start.elapsed();

        let start = Instant::now();
        client.write_all(&encode_request(2, 1, QNAME, &[])).await.unwrap();
        let (status, response_payload) = read_response(&mut client, Duration::from_secs(300)).await;
        let deq_elapsed = start.elapsed();
        assert_eq!(status, 1, "Dequeue 1GB: expected Succeeded");
        assert_eq!(response_payload.len(), META_LEN + SIZE, "dequeue response size mismatch");

        let got = &response_payload[META_LEN..];
        assert_eq!(got.len(), SIZE, "payload length mismatch");
        // Use `==` on raw slices — assert_eq! would dump 1 GB into the test log on failure.
        assert!(got == payload.as_slice(), "1GB payload byte mismatch on round-trip");

        let mbps_enq = (SIZE as f64 / (1024.0 * 1024.0)) / enq_elapsed.as_secs_f64();
        let mbps_deq = (SIZE as f64 / (1024.0 * 1024.0)) / deq_elapsed.as_secs_f64();
        println!(
            "large_payload_roundtrip_1gb: enqueue {:?} ({:.1} MB/s), dequeue {:?} ({:.1} MB/s)",
            enq_elapsed, mbps_enq, deq_elapsed, mbps_deq
        );

        stop_word.notify();
    }
}