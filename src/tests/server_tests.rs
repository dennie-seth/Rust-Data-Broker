#[cfg(test)]
mod tests {
    use std::mem::discriminant;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;
    use tokio::sync::Mutex;
    use tokio::time::timeout;
    use crate::config::Config;
    use crate::net::server::{start_server, Notify, Pool, ServerState};

    async fn wait_for_state(state: Arc<Mutex<ServerState>>,
        expected: ServerState,
        max_wait: Duration) {
        let _ = timeout(max_wait, async {
            loop {
                let state = state.lock().await.clone();
                if discriminant(&state) == discriminant(&expected) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).
            await.
            expect("timed out waiting for server state");
    }
    fn free_local_addr() -> SocketAddrV4 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, address.port())
    }

    #[test]
    fn notify_set_flag() {
        let notify = Notify::new();
        assert!(!notify.notified());
        notify.notify();
        assert!(notify.notified());
    }
    #[tokio::test]
    async fn notify_is_visible_across_tasks() {
        let notify = Arc::new(Notify::new());
        let task_notify = notify.clone();
        let waiter = tokio::spawn(async move {
            for _ in 0..10 {
                if task_notify.notified() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            false
        });

        tokio::time::sleep(Duration::from_millis(10)).await;
        notify.notify();

        let result = waiter.await.unwrap();
        assert!(result, "notify() wasn't observed from another task");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_executes_job() {
        let pool = Pool::new(2, 8).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..10 {
            let counter = counter.clone();
            pool.spawn(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }).await.unwrap();
        }
        let _ = timeout(Duration::from_secs(1), async {
            loop {
                if counter.load(Ordering::SeqCst) == 10 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("timed out waiting for server executes job");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_round_robin() {
        let pool = Pool::new(3, 4).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..30 {
            let counter = counter.clone();
            pool.spawn(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }).await.unwrap();
        }

        let _ = timeout(Duration::from_secs(2), async {
            loop {
                if counter.load(Ordering::SeqCst) == 30 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.expect("timed out waiting for server executes job");
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_transitions_without_clients() {
        let address = free_local_addr();
        let config = Config {
            threads_limit: 1,
            proc_limit: 1,
            wait_limit: 8,
            server_addr: address.ip().to_string(),
            server_port: address.port().to_string(),
        };

        let (state, stop_word) = start_server(config).unwrap();
        wait_for_state(state.clone(), ServerState::Waiting, Duration::from_secs(1)).await;
        stop_word.notify();
        wait_for_state(state.clone(), ServerState::Stopped, Duration::from_secs(1)).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_transitions_with_clients() {
        let address = free_local_addr();
        let config = Config {
            threads_limit: 1,
            proc_limit: 1,
            wait_limit: 8,
            server_addr: address.ip().to_string(),
            server_port: address.port().to_string(),
        };

        let (state, stop_word) = start_server(config).unwrap();
        wait_for_state(state.clone(), ServerState::Waiting, Duration::from_secs(1)).await;

        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"hello world").await.unwrap();

        wait_for_state(state.clone(), ServerState::Busy, Duration::from_secs(1)).await;
        stop_word.notify();
        wait_for_state(state.clone(), ServerState::Stopped, Duration::from_secs(1)).await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn server_accepts_multiple_clients_sequentially() {
        let address = free_local_addr();
        let config = Config {
            threads_limit: 1,
            proc_limit: 1,
            wait_limit: 8,
            server_addr: address.ip().to_string(),
            server_port: address.port().to_string(),
        };

        let (state, stop_word) = start_server(config).unwrap();
        wait_for_state(state.clone(), ServerState::Waiting, Duration::from_secs(1)).await;

        for _ in 0..3 {
            let mut client = TcpStream::connect(address).await.unwrap();
            client.write_all(b"hello world").await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        let state_current = state.lock().await.clone();
        assert!(matches!(state_current, ServerState::Busy | ServerState::Waiting));
        stop_word.notify();
        wait_for_state(state.clone(), ServerState::Stopped, Duration::from_secs(1)).await;
    }
}
