use std::collections::HashMap;
use std::net::{SocketAddrV4, };
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::atomic::Ordering::Relaxed;
use tokio::io::AsyncBufReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex};
use crate::config::Config;

type Job = Pin<Box<dyn Future<Output = ()> + Send>>;
#[derive(Debug)]
struct Pool {
    map : HashMap<usize, std::sync::mpsc::SyncSender<Job>>,
    next_worker: Arc<AtomicUsize>,
    workers: usize,
}
impl Pool {
    fn new(workers: usize, queue_capacity: usize) -> Result<Pool, std::io::Error> {
        println!("Pool::new called, workers={workers}");
        let mut map = HashMap::new();
        for worker_id in 0..workers {
            let (sender, receiver) = std::sync::mpsc::sync_channel::<Job>(queue_capacity);
            map.insert(worker_id, sender);

            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread().
                    enable_all().
                    build().unwrap();
                while let Ok(job) = receiver.recv() {
                    println!("[worker {:?}] got a job", std::thread::current().id());
                    runtime.block_on(job);
                    println!("[worker {:?}] finished job", std::thread::current().id());
                }
            });
        }
        Ok(Pool { map, next_worker: Arc::new(AtomicUsize::new(0)), workers })
    }

    async fn spawn<Fut>(&self, future: Fut) -> Result<(), std::io::Error>
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let worker_id = self.next_worker.fetch_add(1, Relaxed) % self.workers;
        if let Some(sender) = self.map.get(&worker_id) {
            let sender = sender.clone();
            let job = Box::pin(future) as Job;
            tokio::task::spawn_blocking(move || sender.send(job)).await.
                map_err(|_| std::io::ErrorKind::BrokenPipe)?.
                map_err(|_| std::io::ErrorKind::Other)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum ServerState {
    Init,
    Waiting,
    Busy,
    Stopped,
}

#[derive(Debug, Clone)]
struct Addr {
    addr: SocketAddrV4,
}

impl Addr {
    fn new(addr: String, port: String) -> Self {
        Self{
            addr: SocketAddrV4::new(addr.parse().unwrap(), port.parse().unwrap()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notify {
    is_notified: Arc<AtomicBool>,
}

impl Notify {
    fn new() -> Self {
        Self {
            is_notified: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn notify(&self) {
        self.is_notified.store(true, Ordering::Release);
    }
    fn notified(&self) -> bool {
        self.is_notified.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub struct Server {
    state: Arc<Mutex<ServerState>>,
    addr: Addr,
    stop_word: Arc<Notify>,
}
async fn handle_connection(stream: TcpStream, stop_word: Arc<Notify>) {
    let peer_addr = stream.peer_addr().unwrap();
    println!("New connection from {peer_addr}");

    let (reader, _) = tokio::io::split(stream);
    let mut buf_reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    loop {
        if stop_word.notified() {
            break;
        }
        line.clear();
        match buf_reader.read_line(&mut line).await {
            Ok(0) => {
                println!("Client {peer_addr} disconnected");
                break;
            }
            Ok(_) => {
                println!("Got message: {:?}", line);
            }
            Err(e) => {
                println!("Read error: {e}");
                break;
            }
        }
    }
}
async fn spawn_connection(pool: Arc<Pool>, socket: TcpStream, stop_word: Arc<Notify>) -> Result<(), std::io::Error>{
    println!("New connection from {}", socket.peer_addr()?);
    pool.spawn(async move { handle_connection(socket, stop_word.clone()).await }).await
}
impl Server {
    fn new(state: Arc<Mutex<ServerState>>, addr: Addr, stop_word: Arc<Notify>) -> Self {
        Self {
            state,
            addr,
            stop_word,
        }
    }
    pub async fn run(&mut self, config: Config) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(&self.addr.addr).await?;
        let state = self.state.clone();
        let stop_word = self.stop_word.clone();
        let pool = Arc::new(Pool::new(
            config.threads_limit as usize,
            config.wait_limit as usize)?);
        {
            *state.lock().await = ServerState::Waiting;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                if stop_word.notified() {
                    *state.lock().await = ServerState::Stopped;
                    break;
                }

                let accepted = listener.accept().await;
                match accepted.as_ref() {
                    Ok((_, _)) => {
                        let (stream, _) = accepted?;
                        *state.lock().await = ServerState::Busy;
                        spawn_connection(pool.clone(), stream, stop_word.clone()).await?;
                    },
                    Err(ref err) => { println!("accept error = {:?}", err); }
                }
            }
        }
        Ok(())
    }
}

pub fn start_server(config: Config) -> Result<(Arc<Mutex<ServerState>>, Arc<Notify>), Box<dyn std::error::Error>> {
    let state = Arc::new(Mutex::new(ServerState::Init));
    let socket_addr = Addr::new(config.server_addr.to_owned(), config.server_port.to_owned());
    let stop_word = Arc::new(Notify::new());
    let mut server = Server::new(state.clone(), socket_addr.clone(), stop_word.clone());
    
    tokio::spawn(async move {
        let result = server.run(config).await;
        if result.is_err() {
            println!("Server stopped with error: {:?}", result.err().unwrap());
            return;
        }
    });
    Ok((state, stop_word)) 
}
