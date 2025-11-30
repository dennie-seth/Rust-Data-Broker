use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;
use tokio::io::{AsyncReadExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex};
use bytes::BytesMut;
use crate::config::Config;

type Job = Pin<Box<dyn Future<Output = ()> + Send>>;
#[derive(Debug)]
pub(crate) struct Pool {
    map : HashMap<usize, std::sync::mpsc::SyncSender<Job>>,
    next_worker: Arc<AtomicUsize>,
    workers: usize,
}
impl Pool {
    pub(crate) fn new(workers: usize, queue_capacity: usize) -> Result<Pool, std::io::Error> {
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

    pub(crate) async fn spawn<Fut>(&self, future: Fut) -> Result<(), std::io::Error>
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
pub(crate) enum ServerState {
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
pub(crate) struct Notify {
    is_notified: Arc<AtomicBool>,
}
impl Notify {
    pub(crate) fn new() -> Self {
        Self {
            is_notified: Arc::new(AtomicBool::new(false)),
        }
    }
    pub(crate) fn notify(&self) {
        self.is_notified.store(true, Ordering::Release);
    }
    pub(crate) fn notified(&self) -> bool {
        self.is_notified.load(Ordering::Acquire)
    }
}
#[repr(u8)]
#[derive(Debug, Clone)]
enum Request {
    Enqueue = 1,
    Dequeue = 2,
}
impl Request {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Request::Enqueue,
            2 => Request::Dequeue,
            _ => unimplemented!(),
        }
    }
}
#[repr(u8)]
#[derive(Debug, Clone)]
enum Response {
    Succeeded = 1,
    Failed = 2,
}
impl Response {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Response::Succeeded,
            2 => Response::Failed,
            _ => unimplemented!(),
        }
    }
}
#[derive(Debug, Clone)]
pub(crate) struct Server {
    state: Arc<Mutex<ServerState>>,
    addr: Addr,
    clients: Arc<Mutex<HashMap<SocketAddr, BrokerClient>>>,
    stop_word: Arc<Notify>,
}
#[derive(Debug, Clone)]
pub(crate) struct BrokerClient {
    payload_queue: Arc<Mutex<VecDeque<RequestMessage>>>,
}
impl BrokerClient {
    pub(crate) fn new() -> Self {
        Self {
            payload_queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
    pub(crate) async fn enqueue(&mut self, message: &RequestMessage) {
        self.payload_queue.lock().await.push_back(message.clone());
        println!("[worker {:?}] enqueued a message of size {:?}",
                 std::thread::current().id(), message.payload_size);
    }
    pub(crate) async fn dequeue(&mut self) -> Result<RequestMessage, std::io::Error> {
        let message = self.payload_queue.lock().await.pop_front();
        if message.is_some() {
            println!("[worker {:?}] dequeued a message of size {:?}",
                std::thread::current().id(), message.clone().unwrap().payload_size);
            return Ok(message.unwrap())
        }
        Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "no message to dequeue"))
    }
}
#[derive(Debug, Clone)]
pub(crate) struct RequestMessage {
    command: Request,
    payload_size: usize,
    payload: Vec<u8>,
}
impl RequestMessage {
    pub(crate) fn new(bytes: BytesMut) -> Self {
        Self {
            command: Request::from_u8(bytes[0]),
            payload_size: bytes[1] as usize,
            payload: bytes[2..].to_vec(),
        }
    }
}
async fn parse_message(buffer: &mut BytesMut) -> Result<(RequestMessage), std::io::Error> {
    let message;
    loop {
        let payload_size = buffer[1];
        if buffer.len() >= payload_size as usize + 2 {
            message = RequestMessage::new(buffer.split_to(payload_size as usize + 2));
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(message)
}
async fn read_buffer(stream: Arc<Mutex<TcpStream>>, broker_client: Arc<Mutex<BrokerClient>>, stop_word: Arc<Notify>) -> Result<(), std::io::Error>
{
    let mut buffer = BytesMut::with_capacity(1024*4);
    loop {
        if stop_word.notified() {
            break;
        }
        stream.lock().await.read_buf(&mut buffer).await?;
        if buffer.len() >= 2 {
            match Request::from_u8(buffer[0]) {
                Request::Enqueue | Request::Dequeue => {
                    let mut buffer = buffer.clone();
                    let broker_client = broker_client.clone();
                    tokio::spawn(async move {
                        match parse_message(&mut buffer).await {
                            Ok(message) => {
                                broker_client.lock().await.enqueue(&message).await;
                            },
                            Err(err) => {
                                println!("[worker {:?}] parse_message error {:?}", std::thread::current().id(), err);
                            }
                        }
                    });
                }
            }
        }
    }
    Ok(())
}
async fn handle_connection(stream: TcpStream, broker_client: Arc<Mutex<BrokerClient>>, stop_word: Arc<Notify>) {
    let peer_addr = stream.peer_addr().unwrap();
    println!("New connection from {peer_addr}");
    let stream = Arc::new(Mutex::new(stream));
    loop {
        if stop_word.notified() {
            break;
        }
        match read_buffer(stream.clone(), broker_client.clone(), stop_word.clone()).await {
            Ok(_) => {},
            Err(err) => {
                println!("[worker {:?}] read_buffer error {:?}", std::thread::current().id(), err);
            }
        }
    }
}
async fn spawn_connection(pool: Arc<Pool>, socket: TcpStream, broker_client: Arc<Mutex<BrokerClient>>, stop_word: Arc<Notify>) -> Result<(), std::io::Error>{
    println!("New connection from {}", socket.peer_addr()?);
    pool.spawn(async move { handle_connection(socket, broker_client, stop_word.clone()).await }).await
}
impl Server {
    fn new(state: Arc<Mutex<ServerState>>, addr: Addr, stop_word: Arc<Notify>) -> Self {
        Self {
            state,
            addr,
            clients: Arc::new(Mutex::new(HashMap::new())),
            stop_word,
        }
    }
    pub(crate) async fn run(&mut self, config: Config) -> Result<(), std::io::Error> {
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

                let accept_result =
                    tokio::time::timeout(
                        tokio::time::Duration::from_millis(50),
                        listener.accept(),
                    ).await;
                match accept_result {
                    Err(_elapsed) => {
                        continue;
                    }
                    Ok(Err(e)) => {
                        println!("accept error = {:?}", e);
                        continue;
                    }
                    Ok(Ok((stream, client_addr))) => {
                        *state.lock().await = ServerState::Busy;
                        let broker_client = Arc::new(Mutex::new(BrokerClient::new()));
                        self.clients.lock().await.insert(client_addr, broker_client.lock().await.clone());
                        spawn_connection(pool.clone(), stream, broker_client, stop_word.clone()).await?;
                    }
                }
            }
        }
        Ok(())
    }
}
pub(crate) fn start_server(config: Config) -> Result<(Arc<Mutex<ServerState>>, Arc<Notify>), Box<dyn std::error::Error>> {
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
