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
        }
    }

    fn encode_request(command: u8, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1 + 8 + payload.len());
        buf.push(command);
        buf.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        buf.extend_from_slice(payload);
        buf
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
        let stop_word = start_server(make_config(address)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        TcpStream::connect(address).await.unwrap();

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_enqueue_responds_success() {
        let address = free_local_addr();
        let stop_word = start_server(make_config(address)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&encode_request(1, b"hello world")).await.unwrap();

        let mut response = [0u8; 9];
        timeout(Duration::from_secs(1), client.read_exact(&mut response))
            .await.expect("timed out reading response").unwrap();
        assert_eq!(response[0], 1, "expected Succeeded (1)");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_dequeue_empty_responds_failure() {
        let address = free_local_addr();
        let stop_word = start_server(make_config(address)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(&encode_request(2, b"")).await.unwrap();

        let mut response = [0u8; 9];
        timeout(Duration::from_secs(1), client.read_exact(&mut response))
            .await.expect("timed out reading response").unwrap();
        assert_eq!(response[0], 2, "expected Failed (2)");

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_enqueue_then_dequeue_returns_message() {
        let address = free_local_addr();
        let stop_word = start_server(make_config(address)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = TcpStream::connect(address).await.unwrap();
        let payload = b"hello world";

        // Enqueue
        client.write_all(&encode_request(1, payload)).await.unwrap();
        let mut response = [0u8; 9];
        timeout(Duration::from_secs(1), client.read_exact(&mut response))
            .await.expect("timed out on enqueue response").unwrap();
        assert_eq!(response[0], 1, "enqueue: expected Succeeded");

        // Dequeue
        client.write_all(&encode_request(2, b"")).await.unwrap();
        let payload_size = payload.len() as u64;
        let mut response = vec![0u8; 9 + payload_size as usize];
        timeout(Duration::from_secs(1), client.read_exact(&mut response))
            .await.expect("timed out on dequeue response").unwrap();
        assert_eq!(response[0], 1, "dequeue: expected Succeeded");
        assert_eq!(&response[9..], payload);

        stop_word.notify();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_accepts_multiple_clients() {
        let address = free_local_addr();
        let stop_word = start_server(make_config(address)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut handles = vec![];
        for i in 0..3u8 {
            handles.push(tokio::spawn(async move {
                let mut client = TcpStream::connect(address).await.unwrap();
                let payload = vec![i; 4];
                client.write_all(&encode_request(1, &payload)).await.unwrap();
                let mut response = [0u8; 9];
                timeout(Duration::from_secs(1), client.read_exact(&mut response))
                    .await.expect("timed out").unwrap();
                assert_eq!(response[0], 1);
            }));
        }
        for h in handles { h.await.unwrap(); }

        stop_word.notify();
    }
}