use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::Arc;
// TODO(cleanup): AtomicBool is imported but unused since Notify was rewritten.
use std::sync::atomic::{AtomicUsize};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex};
use bytes::BytesMut;
use tokio::net::tcp::OwnedWriteHalf;
use crate::config::Config;

type Job = Pin<Box<dyn Future<Output = ()> + Send>>;
#[derive(Debug)]
pub(crate) struct Pool {
    map : HashMap<usize, std::sync::mpsc::SyncSender<Job>>,
    // TODO(cleanup): next_worker and workers are unused since pressure-based dispatch replaced
    // round-robin. Remove them (and the AtomicUsize/Ordering imports above).
    pressure: Vec<Arc<AtomicUsize>>,
}
impl Pool {
    pub(crate) fn new(workers: usize, queue_capacity: usize) -> Result<Pool, std::io::Error> {
        println!("Pool::new called, workers={workers}");
        let mut map = HashMap::new();
        let mut pressure = Vec::with_capacity(workers);

        for worker_id in 0..workers {
            let counter = Arc::new(AtomicUsize::new(0));
            pressure.push(counter.clone());

            let (sender, receiver) = std::sync::mpsc::sync_channel::<Job>(queue_capacity);
            map.insert(worker_id, sender);

            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread().
                    enable_all().
                    build().unwrap();
                while let Ok(job) = receiver.recv() {
                    println!("[worker {:?}] got a job", std::thread::current().id());
                    runtime.block_on(job);
                    counter.fetch_sub(1, Relaxed);
                    println!("[worker {:?}] finished job", std::thread::current().id());
                }
            });
        }
        Ok(Pool { map, pressure })
    }

    pub(crate) async fn spawn<Fut>(&self, future: Fut) -> Result<(), std::io::Error>
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        let worker_id = self.pressure.
            iter().
            enumerate().
            min_by_key(|(_, counter)| counter.load(Relaxed)).
            map(|(id, _)| id).
            unwrap_or(0);

        self.pressure[worker_id].fetch_add(1, Relaxed);

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
struct Addr {
    addr: SocketAddrV4,
}
impl Addr {
    fn new(addr: String, port: String) -> Result<Self, std::io::Error> {
            Ok(Self{
            addr: SocketAddrV4::new(
                addr.parse().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?,
                port.parse().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?,
            ),
        })
    }
}
#[derive(Debug, Clone)]
pub(crate) struct Notify {
    is_notified: Arc<tokio::sync::Notify>,
}
impl Notify {
    pub(crate) fn new() -> Self {
        Self {
            is_notified: Arc::new(tokio::sync::Notify::new()),
        }
    }
    pub(crate) fn notify(&self) {
        self.is_notified.notify_waiters();
    }
    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.is_notified.notified()
    }
}
#[repr(u8)]
#[derive(Debug, Clone)]
pub(crate) enum Request {
    Enqueue = 1,
    Dequeue = 2,
}
impl Request {
    // TODO(bug): Unknown byte still hits `unimplemented!()` which panics and kills the worker
    // thread. Replace with `Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "..."))`.
    pub(crate) fn from_u8(value: u8) -> Result<Self, std::io::Error> {
        match value {
            1 => Ok(Request::Enqueue),
            2 => Ok(Request::Dequeue),
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Unknown request type")),
        }
    }
}
#[repr(u8)]
#[derive(Debug, Clone)]
pub(crate) enum Response {
    Succeeded = 1,
    Failed = 2,
}
impl Response {
    // TODO(bug): Same unimplemented!() panic as Request::from_u8. Return Err(...) instead.
    pub(crate) fn from_u8(value: u8) -> Result<Self, std::io::Error> {
        match value {
            1 => Ok(Response::Succeeded),
            2 => Ok(Response::Failed),
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Unknown request type")),
        }
    }
}
#[derive(Debug, Clone)]
pub(crate) struct Server {
    addr: Addr,
    // TODO(design): This map is currently write-only — nothing reads it to route messages
    // between clients. For a real broker you'd need a shared global queue (or topic map) that
    // any client can enqueue into and any other client can dequeue from.
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
    pub(crate) async fn enqueue(&mut self, message: &RequestMessage) -> Result<(), std::io::Error> {
        self.payload_queue.lock().await.push_back(message.clone());
        println!("[worker {:?}] enqueued a message of size {:?}",
                 std::thread::current().id(), message.payload_size);
        Ok(())
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
    payload_size: u64,
    payload: Vec<u8>,
}
impl RequestMessage {
    pub(crate) fn new(bytes: BytesMut) -> Self {
        Self {
            command: Request::from_u8(bytes[0]).unwrap(),
            payload_size: u64::from_be_bytes(bytes[1..9].try_into().unwrap()),
            payload: bytes[9..].to_vec(),
        }
    }
}
#[derive(Debug, Clone)]
pub(crate) struct ResponseMessage {
    status: Response,
    payload_size: u64,
    payload: Vec<u8>,
}
impl ResponseMessage {
    pub(crate) fn new(status: Response, payload: Vec<u8>) -> Self {
        Self {
            status,
            payload_size: payload.len() as u64,
            payload
        }
    }
    pub(crate) fn to_u8(&self) -> Vec<u8> {
        let mut bytes: Vec<u8> = Vec::new();
        bytes.push(self.status.clone() as u8);
        bytes.append(&mut self.payload_size.to_be_bytes().to_vec());
        bytes.append(&mut self.payload.clone());
        bytes
    }
}
// TODO(bug): This function receives a *cloned* BytesMut (see read_buffer below). No new data
// is ever written into that clone, so if the clone doesn't already hold the full message the
// sleep(10ms) loop spins forever, leaking the spawned task.
// Fix: do not clone the buffer. Instead, keep reading from the real stream until enough bytes
// have arrived, then parse. The simplest approach is to read header (9 bytes) first, then read
// exactly payload_size more bytes directly from the stream before constructing RequestMessage.
//
// TODO(bug): `buffer[1..9]` is accessed without first checking that buffer.len() >= 9.
// If called with fewer than 9 bytes this panics. Add a guard:
//   if buffer.len() < 9 { return Err(...); }
// TODO(suggestion): This function duplicates the logic of BrokerClient::dequeue() by
// directly accessing payload_queue instead of calling broker_client.lock().await.dequeue().
// Consolidate into one code path to avoid diverging behaviour and missing log output.
async fn send_message(stream: Arc<Mutex<OwnedWriteHalf>>, broker_client: Arc<Mutex<BrokerClient>>) -> Result<(), std::io::Error> {
    let message = broker_client.lock().await.dequeue().await?;
    if message.payload_size > 0 {
        let message = ResponseMessage::new(Response::Succeeded, message.payload);
        stream.lock().await.write_all(&message.to_u8()).await?;
        return Ok(());
    }
    let message = ResponseMessage::new(Response::Failed, vec!());
    stream.lock().await.write_all(&message.to_u8()).await?;
    Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "no message to dequeue"))
}
async fn read_buffer(stream: TcpStream, broker_client: Arc<Mutex<BrokerClient>>, stop_word: Arc<Notify>) -> Result<(), std::io::Error>
{
    let mut buffer = BytesMut::with_capacity(1024*4);
    let (mut stream_read, stream_write) = stream.into_split();
    let stream_write = Arc::new(Mutex::new(stream_write));


    loop {
        tokio::select! {
            _ = stop_word.notified() => { break; },
            result = stream_read.read_buf(&mut buffer) => { result?; },
        }

        if buffer.len() >= 9 {
            let payload_size = u64::from_be_bytes(buffer[1..9].try_into().unwrap());
            if buffer.len() < payload_size as usize + 9 {
                continue;
            }
            let command = Request::from_u8(buffer[0])?;
            let message = RequestMessage::new(buffer.split_to(9 + payload_size as usize));

            match command{
                Request::Enqueue => {
                    // TODO(bug): `buffer.clone()` creates a snapshot of the current bytes.
                    // No further reads are written into this clone, so parse_message will spin
                    // forever if the snapshot doesn't already hold the full payload.
                    // Fix: remove the clone and parse directly from `buffer`, or restructure to
                    // read from the stream until the full message is available before spawning.
                    //
                    // TODO(bug): The original `buffer` is never consumed (split_to is called
                    // only on the clone). After this match arm, buffer still holds the same bytes,
                    // so the next read_buf call appends to already-processed data, breaking framing.
                    // Fix: call buffer.advance(n) or buffer.split_to(n) on the *original* buffer
                    // once the full message has been identified.

                    let broker_client = broker_client.clone();
                    let writer = stream_write.clone();

                    tokio::spawn(async move {
                        match broker_client.lock().await.enqueue(&message).await {
                            Ok(_) => {
                                let response = ResponseMessage::new(Response::Succeeded, vec!());
                                send_response(writer, &response).await;
                            },
                            Err(err) => {
                                let response = ResponseMessage::new(Response::Failed, vec!());
                                send_response(writer, &response).await;
                                println!("[worker {:?}] parse_message error {:?}", std::thread::current().id(), err);
                            }
                        }
                    });
                }
                Request::Dequeue => {
                    let broker_client = broker_client.clone();
                    let writer = stream_write.clone();

                    tokio::spawn(async move {
                        match send_message(writer, broker_client).await {
                            Ok(_) => {},
                            Err(err) => {
                                println!("[worker {:?}] send_message error {:?}", std::thread::current().id(), err);
                            }
                        }
                    });
                }
            }
        }
    }
    Ok(())
}

async fn send_response(stream: Arc<Mutex<OwnedWriteHalf>>, response: &ResponseMessage) {
    match stream.lock().await.write_all(&response.to_u8()).await {
        Ok(_) => (),
        Err(err) => {
            println!("[worker {:?}] response error {:?}", std::thread::current().id(), err);
        }
    }
}

impl Server {
    fn new(addr: Addr, stop_word: Arc<Notify>) -> Self {
        Self {
            addr,
            clients: Arc::new(Mutex::new(HashMap::new())),
            stop_word,
        }
    }
    pub(crate) async fn run(&mut self, config: Config) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(&self.addr.addr).await?;
        let stop_word = self.stop_word.clone();
        let pool = Arc::new(Pool::new(
            config.threads_limit as usize,
            config.wait_limit as usize)?);
        {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                tokio::select! {
                    _ = stop_word.notified() => { break; },
                    accept_result = tokio::time::timeout(Duration::from_millis(50), listener.accept()) => {
                        match accept_result {
                            Err(_elapsed) => {
                                continue;
                            }
                            Ok(Err(e)) => {
                                println!("accept error = {:?}", e);
                                continue;
                            }
                            Ok(Ok((stream, client_addr))) => {
                                // TODO(design): ServerState::Busy/Waiting no longer maps to reality —
                                // connections are now handled concurrently so the server is never
                                // exclusively "busy". Replace with a connection counter (AtomicUsize)
                                // or remove ServerState entirely.
                                let broker_client = Arc::new(Mutex::new(BrokerClient::new()));
                                self.clients.lock().await.insert(client_addr, broker_client.lock().await.clone());
                                // TODO(bug): spawn_connection result is silently discarded. If
                                // peer_addr() fails the error is swallowed. Propagate or log it.
                                _ = tokio::spawn(self.clone().spawn_connection(pool.clone(), stream, broker_client, stop_word.clone(), client_addr));
                            }
                        }},
                }
            }
        }
        Ok(())
    }
    // TODO(design): Each connection gets its own isolated BrokerClient queue. Enqueue from client A
    // only goes into A's queue, and Dequeue from A only reads from A's queue. This makes it a
    // per-connection buffer, not a message broker. For real pub/sub or work-queue semantics you need
    // a shared global queue (or named topic map) that is independent of individual connections.
    async fn handle_connection(self, stream: TcpStream, broker_client: Arc<Mutex<BrokerClient>>, stop_word: Arc<Notify>, addr: SocketAddr) {
        if let Err(err) = read_buffer(stream, broker_client.clone(), stop_word.clone()).await {
            println!("[worker {:?}] read_buffer error {:?}", std::thread::current().id(), err);
        }
        self.clients.lock().await.remove(&addr);
    }
    async fn spawn_connection(self, pool: Arc<Pool>, socket: TcpStream, broker_client: Arc<Mutex<BrokerClient>>, stop_word: Arc<Notify>, addr: SocketAddr) -> Result<(), std::io::Error>{
        println!("New connection from {}", addr);
        pool.spawn(async move { self.handle_connection(socket, broker_client, stop_word.clone(), addr).await }).await
    }
}
pub(crate) fn start_server(config: Config) -> Result<Arc<Notify>, Box<dyn std::error::Error>> {
    let socket_addr = Addr::new(config.server_addr.to_owned(), config.server_port.to_owned());
    let stop_word = Arc::new(Notify::new());
    let mut server = Server::new(socket_addr?, stop_word.clone());
    
    tokio::spawn(async move {
        let result = server.run(config).await;
        if result.is_err() {
            println!("Server stopped with error: {:?}", result.err().unwrap());
            return;
        }
    });
    Ok(stop_word)
}
