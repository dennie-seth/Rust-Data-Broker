#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::time::timeout;
    use crate::config::Config;
    use crate::net::server::{start_server, Notify, Pool};

    fn free_local_addr() -> SocketAddrV4 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, address.port())
    }

    fn make_config(address: SocketAddrV4) -> Config {
        Config {
            threads_limit: 2,
            proc_limit: 1,
            wait_limit: 8,
            server_addr: address.ip().to_string(),
            server_port: address.port().to_string(),
            queue_names: vec![],
        }
    }

    /// Encodes a request using the full wire protocol:
    /// [1 byte command][16 bytes client_id][8 bytes payload_size][64 bytes queue_name][payload]
    ///
    /// Note: queue_name is null-padded to 64 bytes. Pre-configured queues (from Config::queue_names)
    /// are stored without padding and will NOT match wire-format lookups — always use CreateQ (3)
    /// in tests to create queues so both sides use the same padded key.
    fn encode_request(command: u8, client_id: u128, queue_name: &str, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
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

    async fn read_response(client: &mut TcpStream) -> Vec<u8> {
        let mut header = [0u8; 9];
        timeout(Duration::from_secs(1), client.read_exact(&mut header))
            .await.expect("timed out reading response header").unwrap();
        let payload_size = u64::from_be_bytes(header[1..9].try_into().unwrap()) as usize;
        let mut response = header.to_vec();
        if payload_size > 0 {
            let mut payload = vec![0u8; payload_size];
            timeout(Duration::from_secs(1), client.read_exact(&mut payload))
                .await.expect("timed out reading response payload").unwrap();
            response.extend_from_slice(&payload);
        }
        response
    }

    // --- Notify tests ---

    #[tokio::test]
    async fn notify_wakes_waiter() {
        let notify = Arc::new(Notify::new());
        let n = notify.clone();
        // Waiter must be registered before notify() is called,
        // since notify_waiters() only wakes *current* waiters.
        let waiter = tokio::spawn(async move {
            n.notified().await;
            true
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        notify.notify();
        let result = timeout(Duration::from_secs(1), waiter)
            .await.expect("timed out").unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn notify_wakes_multiple_waiters() {
        let notify = Arc::new(Notify::new());
        let mut handles = vec![];
        for _ in 0..3 {
            let n = notify.clone();
            handles.push(tokio::spawn(async move { n.notified().await }));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        notify.notify();
        for h in handles {
            timeout(Duration::from_secs(1), h)
                .await.expect("timed out").unwrap();
        }
    }

    // --- Pool tests ---

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_executes_job() {
        let pool = Pool::new(2, 8).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..10 {
            let counter = counter.clone();
            pool.spawn(async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }).await.unwrap();
        }
        timeout(Duration::from_secs(1), async {
            loop {
                if counter.load(Ordering::SeqCst) == 10 { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("timed out waiting for jobs to complete");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_dispatches_all_jobs() {
        let pool = Pool::new(3, 4).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..30 {
            let counter = counter.clone();
            pool.spawn(async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }).await.unwrap();
        }
        timeout(Duration::from_secs(2), async {
            loop {
                if counter.load(Ordering::SeqCst) == 30 { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("timed out waiting for jobs to complete");
    }

    // --- Server tests ---

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_accepts_connection() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        TcpStream::connect(address).await.unwrap();

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_create_queue_responds_success() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&encode_request(3, 1, "myqueue", &[])).await.unwrap();

        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded (1)");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_create_queue_duplicate_responds_failure() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "myqueue", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "first CreateQ: expected Succeeded");

        client.write_all(&encode_request(3, 1, "myqueue", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 2, "duplicate CreateQ: expected Failed");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_enqueue_responds_success() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "myqueue", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(1, 1, "myqueue", b"hello world")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue: expected Succeeded");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_dequeue_empty_responds_failure() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "myqueue", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(2, 1, "myqueue", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 2, "Dequeue empty: expected Failed");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_enqueue_then_dequeue_returns_message() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();
        let payload = b"hello world";

        client.write_all(&encode_request(3, 1, "myqueue", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(1, 1, "myqueue", payload)).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue: expected Succeeded");

        client.write_all(&encode_request(2, 1, "myqueue", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue: expected Succeeded");
        // Meta is prepended: [16 id][16 publisher_id][8 timestamp][16 locked_by] = 56 bytes
        assert_eq!(&response[9 + 56..], payload, "Dequeue: payload mismatch");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_accepts_multiple_clients() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Create a shared queue
        {
            let mut setup = TcpStream::connect(address).await.unwrap();
            setup.write_all(&encode_request(3, 0, "shared", &[])).await.unwrap();
            let response = read_response(&mut setup).await;
            assert_eq!(response[0], 1, "CreateQ: expected Succeeded");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut handles = vec![];
        for i in 1..=3u128 {
            handles.push(tokio::spawn(async move {
                let mut client = TcpStream::connect(address).await.unwrap();
                let payload = vec![i as u8; 4];
                client.write_all(&encode_request(1, i, "shared", &payload)).await.unwrap();
                let response = read_response(&mut client).await;
                assert_eq!(response[0], 1, "client {i}: Enqueue expected Succeeded");
            }));
        }
        for h in handles { h.await.unwrap(); }

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_delete_queue_responds_success() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "delq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(4, 1, "delq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "DeleteQ: expected Succeeded");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_delete_queue_nonexistent_responds_failure() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(4, 1, "noqueue", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 2, "DeleteQ nonexistent: expected Failed");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_list_messages_returns_metadata() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "listq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(1, 1, "listq", b"msg1")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue 1: expected Succeeded");

        client.write_all(&encode_request(1, 1, "listq", b"msg2")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue 2: expected Succeeded");

        // ListM = 5
        client.write_all(&encode_request(5, 1, "listq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "ListM: expected Succeeded");
        // Each Meta entry is 56 bytes (16 id + 16 publisher_id + 8 timestamp + 16 locked_by)
        let payload = &response[9..];
        assert_eq!(payload.len() % 56, 0, "ListM payload should be multiple of 56 bytes");
        assert_eq!(payload.len() / 56, 2, "ListM should return 2 entries");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_succeeded_acks_message() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "ackq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(1, 1, "ackq", b"hello")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue: expected Succeeded");

        // Dequeue locks the message to client 1
        client.write_all(&encode_request(2, 1, "ackq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue: expected Succeeded");

        // Succeeded (7) acks the locked message
        client.write_all(&encode_request(7, 1, "ackq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Succeeded: expected Succeeded");

        // Queue should now be empty — dequeue again should fail
        client.write_all(&encode_request(2, 1, "ackq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 2, "Dequeue after ack: expected Failed (empty)");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_failed_nacks_and_unlocks_message() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "nackq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(1, 1, "nackq", b"retry me")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue: expected Succeeded");

        // Dequeue locks the message to client 1
        client.write_all(&encode_request(2, 1, "nackq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue: expected Succeeded");

        // Failed (8) nacks — unlocks the message
        client.write_all(&encode_request(8, 1, "nackq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Failed: expected Succeeded");

        // Message should be available again — dequeue should succeed
        client.write_all(&encode_request(2, 1, "nackq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue after nack: expected Succeeded");
        assert_eq!(&response[9 + 56..], b"retry me", "payload should match original");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_delete_message_by_id() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "delmq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(1, 1, "delmq", b"to delete")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue: expected Succeeded");

        // Dequeue to get the message ID from meta
        client.write_all(&encode_request(2, 1, "delmq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue: expected Succeeded");
        let message_id = &response[9..9 + 16]; // first 16 bytes of meta = message ID

        // DeleteM (6) with message_id as payload
        client.write_all(&encode_request(6, 1, "delmq", message_id)).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "DeleteM: expected Succeeded");

        // Queue should now be empty
        client.write_all(&encode_request(2, 1, "delmq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 2, "Dequeue after delete: expected Failed (empty)");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_requeue_moves_message_to_end() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "reqq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(1, 1, "reqq", b"first")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue 1: expected Succeeded");

        client.write_all(&encode_request(1, 1, "reqq", b"second")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue 2: expected Succeeded");

        // Dequeue to get first message ID
        client.write_all(&encode_request(2, 1, "reqq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue: expected Succeeded");
        let first_msg_id = response[9..9 + 16].to_vec();

        // Requeue (9) the first message — payload is the 16-byte message ID
        client.write_all(&encode_request(9, 1, "reqq", &first_msg_id)).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Requeue: expected Succeeded");

        // Next dequeue should return "second" (the original second message)
        client.write_all(&encode_request(2, 1, "reqq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue after requeue: expected Succeeded");
        assert_eq!(&response[9 + 56..], b"second", "should get 'second' next");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_update_message_changes_payload() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "updq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(1, 1, "updq", b"original")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue: expected Succeeded");

        // Dequeue to get the message ID from meta
        client.write_all(&encode_request(2, 1, "updq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue: expected Succeeded");
        let message_id = &response[9..9 + 16];

        // UpdateM (10) — payload is [16-byte message_id][new payload]
        let mut update_payload = message_id.to_vec();
        update_payload.extend_from_slice(b"updated");
        client.write_all(&encode_request(10, 1, "updq", &update_payload)).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "UpdateM: expected Succeeded");

        // Ack the locked message so we can re-dequeue
        client.write_all(&encode_request(8, 1, "updq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Failed (nack/unlock): expected Succeeded");

        // Dequeue again — should get updated payload
        client.write_all(&encode_request(2, 1, "updq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue after update: expected Succeeded");
        assert_eq!(&response[9 + 56..], b"updated", "payload should be updated");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_update_message_too_short_payload_fails() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "updq2", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        // UpdateM with payload shorter than 16 bytes (no message_id)
        client.write_all(&encode_request(10, 1, "updq2", b"short")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 2, "UpdateM too short: expected Failed");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_delete_other_message_preserves_held_lock() {
        // Regression: DeleteM on a message the client does NOT hold a lock on must
        // not drop the client's lock on a different message (previously it either
        // dropped the unrelated lock or fell through and deleted the locked message too).
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "mixq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        client.write_all(&encode_request(1, 1, "mixq", b"m1")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue m1: expected Succeeded");

        // Dequeue locks m1 to client 1.
        client.write_all(&encode_request(2, 1, "mixq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Dequeue: expected Succeeded");
        let m1_id = response[9..9 + 16].to_vec();

        client.write_all(&encode_request(1, 1, "mixq", b"m2")).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Enqueue m2: expected Succeeded");

        // ListM to find m2's id (the one that isn't m1).
        client.write_all(&encode_request(5, 1, "mixq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "ListM: expected Succeeded");
        let payload = &response[9..];
        assert_eq!(payload.len(), 56 * 2, "expected 2 meta entries");
        let m2_id = if &payload[0..16] == m1_id.as_slice() {
            payload[56..56 + 16].to_vec()
        } else {
            payload[0..16].to_vec()
        };

        // DeleteM on m2 while client 1 still holds the lock on m1.
        client.write_all(&encode_request(6, 1, "mixq", &m2_id)).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "DeleteM m2: expected Succeeded");

        // Client 1's lock on m1 must still be valid — Succeeded(7) should ack m1.
        client.write_all(&encode_request(7, 1, "mixq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "Succeeded ack of m1: expected Succeeded (lock preserved)");

        // Queue is now empty — dequeue fails.
        client.write_all(&encode_request(2, 1, "mixq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 2, "Dequeue after empty: expected Failed");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_update_queue_nonexistent_responds_failure() {
        // UpdateQ (11) against a queue that doesn't exist must return Failed.
        // Also guards the payload-size guard (< 1 byte → Failed).
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        // Send UpdateQ with a 1-byte flags payload (no flags set) on a queue that doesn't exist.
        client.write_all(&encode_request(11, 1, "missingq", &[0u8])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 2, "UpdateQ on missing queue: expected Failed");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_update_queue_valid_config_responds_success() {
        // UpdateQ (11) with a valid NetQueueConfig payload must return Succeeded.
        // Wire format: [1 byte flags][auto_success: 1 byte if flag 0x01 set][success_timeout: 8 BE bytes if flag 0x02 set]
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        client.write_all(&encode_request(3, 1, "cfgok", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        // flags=0b11 → both fields present; auto_success=true, success_timeout=5 (u64 BE).
        let mut payload = Vec::new();
        payload.push(0b11u8);
        payload.push(1u8); // auto_success = true
        payload.extend_from_slice(&5u64.to_be_bytes());

        client.write_all(&encode_request(11, 1, "cfgok", &payload)).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "UpdateQ valid config: expected Succeeded");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_update_queue_empty_payload_responds_failure() {
        // UpdateQ (11) with payload_size < 1 must return Failed.
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, _) = start_server(make_config(address), drained).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();

        // Create the queue so we're past the get_queue check.
        client.write_all(&encode_request(3, 1, "cfgq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 1, "CreateQ: expected Succeeded");

        // UpdateQ with an empty payload — the handler rejects payload_size < 1.
        client.write_all(&encode_request(11, 1, "cfgq", &[])).await.unwrap();
        let response = read_response(&mut client).await;
        assert_eq!(response[0], 2, "UpdateQ empty payload: expected Failed");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_drain_fires_when_accept_stops() {
        // Directly exercises the drain path: once stop_accepting is notified, Server::run
        // exits its accept loop, waits for active_connections to hit 0, and fires drained.
        // Bypasses the ctrl+c → run_shutdown_loop glue because on Windows the test can't
        // reliably deliver a CTRL_C_EVENT to Tokio's signal handler.
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (_stop_word, stop_accepting) =
            start_server(make_config(address), drained.clone()).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let drained_clone = drained.clone();
        let drain_waiter = tokio::spawn(async move { drained_clone.notified().await });
        tokio::time::sleep(Duration::from_millis(5)).await;

        stop_accepting.notify();

        timeout(Duration::from_secs(5), drain_waiter)
            .await.expect("server did not drain within 5 seconds")
            .unwrap();
    }
}