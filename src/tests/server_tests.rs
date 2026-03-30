#[cfg(test)]
mod tests {
    use std::collections::HashMap;
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
        assert_eq!(&response[9..], payload, "Dequeue: payload mismatch");

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
    async fn server_graceful_shutdown_drains_connections() {
        let address = free_local_addr();
        let drained = Arc::new(Notify::new());
        let (stop_word, stop_accepting) =
            start_server(make_config(address), drained.clone()).unwrap();

        // Spawn the real shutdown loop from main.rs — no logic is duplicated here.
        // It handles the ctrl+c signal internally and drives stop_accepting / stop_word.
        let mut env_args = HashMap::new();
        env_args.insert("--shutdown_timeout".to_string(), "30".to_string());
        let drained_for_loop = drained.clone();
        tokio::spawn(async move {
            crate::run_shutdown_loop(stop_word, stop_accepting, drained_for_loop, env_args)
                .await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(50)).await; // let server start and ctrl_c() register

        // Register drain waiter before sending the signal so it catches the notification.
        let drained_clone = drained.clone();
        let drain_waiter = tokio::spawn(async move { drained_clone.notified().await });
        tokio::time::sleep(Duration::from_millis(5)).await;

        send_ctrl_c();

        timeout(Duration::from_secs(30), drain_waiter)
            .await.expect("server did not drain within 30 seconds")
            .unwrap();
    }

    fn send_ctrl_c() {
        #[cfg(windows)]
        unsafe extern "system" {
            fn GenerateConsoleCtrlEvent(dwCtrlEvent: u32, dwProcessGroupId: u32) -> i32;
        }
        #[cfg(windows)]
        unsafe { GenerateConsoleCtrlEvent(0, 0); }
        #[cfg(unix)]
        unsafe extern "C" {
            fn raise(sig: i32) -> i32;
        }
        #[cfg(unix)]
        unsafe { raise(2); } // SIGINT
    }
}