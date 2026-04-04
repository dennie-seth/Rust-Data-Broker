use std::collections::{HashMap};
use std::io;
use std::net::{SocketAddr, SocketAddrV4};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use bytes::BytesMut;
use tokio::net::tcp::OwnedWriteHalf;
use uuid::Uuid;
use crate::config::Config;
use crate::net::queue::Queue;

static COMMAND_SIZE: usize = 1usize;
static PAYLOAD_SIZE: usize = 8usize;
static CLIENT_ID_SIZE: usize = 16usize;
static QUEUE_NAME_SIZE: usize = 64usize;

type Job = Pin<Box<dyn Future<Output = ()> + Send>>;
#[derive(Debug)]
pub(crate) struct Pool {
    map : HashMap<usize, std::sync::mpsc::SyncSender<Job>>,
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

        let sender = self.map.get(&worker_id).ok_or_else(|| { self.pressure[worker_id].fetch_sub(1, Relaxed); io::Error::new(io::ErrorKind::BrokenPipe, "worker not found") } )?;
        let sender = sender.clone();
        let job = Box::pin(future) as Job;
        tokio::task::spawn_blocking( move | | sender.send(job)).await.
        map_err( | _ | std::io::ErrorKind::BrokenPipe) ?.
        map_err( | _ | std::io::ErrorKind::Other) ?;
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
    CreateQ = 3,
    DeleteQ = 4,
    ListM = 5,
    DeleteM = 6,
    Succeeded = 7,
    Failed = 8,
    Requeue = 9,
    UpdateM = 10,
}
impl Request {
    pub(crate) fn from_u8(value: u8) -> Result<Self, std::io::Error> {
        match value {
            1 => Ok(Request::Enqueue),
            2 => Ok(Request::Dequeue),
            3 => Ok(Request::CreateQ),
            4 => Ok(Request::DeleteQ),
            5 => Ok(Request::ListM),
            6 => Ok(Request::DeleteM),
            7 => Ok(Request::Succeeded),
            8 => Ok(Request::Failed),
            9 => Ok(Request::Requeue),
            10 => Ok(Request::UpdateM),
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
    queue: Arc<RwLock<HashMap<Uuid, Arc<Mutex<Queue>>>>>,
    queue_names: Arc<RwLock<HashMap<String, Uuid>>>,
    stop_word: Arc<Notify>,
    stop_accepting: Arc<Notify>,
    active_connections: Arc<AtomicUsize>,
}
#[derive(Debug, Clone)]
pub(crate) struct RequestMessage {
    // TODO(cleanup): command is stored but never read after construction. Remove the field
    // or use it (e.g. for logging which command a queued message originated from).
    command: Request,
    payload_size: u64,
    payload: Vec<u8>,
}
impl RequestMessage {
    pub(crate) fn new(command: &Request, bytes: BytesMut) -> Result<Self, std::io::Error> {
        Ok(Self {
            command:  command.to_owned(),
            payload_size: u64::from_be_bytes(bytes[COMMAND_SIZE+CLIENT_ID_SIZE..COMMAND_SIZE+CLIENT_ID_SIZE+PAYLOAD_SIZE].try_into().unwrap()),
            payload: bytes[COMMAND_SIZE+CLIENT_ID_SIZE+PAYLOAD_SIZE+QUEUE_NAME_SIZE..].to_vec(),
        })
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
impl Server {
    fn new(addr: Addr, stop_word: Arc<Notify>, stop_accepting: Arc<Notify>) -> Self {

        Self {
            addr,
            queue: Arc::new(RwLock::new(HashMap::new())),
            stop_word,
            stop_accepting,
            active_connections: Arc::new(AtomicUsize::new(0_usize)),
            queue_names: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    pub(crate) async fn run(&mut self, config: Config, drained: &Arc<Notify>) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(&self.addr.addr).await?;
        let stop_word = self.stop_word.clone();
        let pool = Arc::new(Pool::new(
            config.threads_limit as usize,
            config.wait_limit as usize)?);
        for queue_name in config.queue_names {
            let hash = Uuid::new_v4();
            self.queue_names.write().await.insert(queue_name, hash);
            self.queue.write().await.insert(hash, Arc::new(Mutex::new(Queue::new())));
        }
        {
            loop {
                tokio::select! {
                    _ = self.stop_accepting.notified() => { break; },
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, client_addr)) => {
                                let server = self.clone();
                                let pool = pool.clone();
                                let stop_word = stop_word.clone();
                                tokio::spawn(async move{
                                    match server.spawn_connection(pool, stream, stop_word.clone(), client_addr).await {
                                        Ok(_) => {},
                                        Err(err) => {
                                            println!("[worker {:?}] spawn_connection error {:?}", std::thread::current().id(), err);
                                        }
                                    }
                                });
                            },
                            Err(err) => { println!("[worker {:?}] error {:?}", std::thread::current().id(), err); },
                        }},
                }
            }
            loop {
                if self.active_connections.load(Relaxed) == 0 { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            drained.notify();
        }
        Ok(())
    }
    async fn handle_connection(&self, stream: TcpStream, stop_word: Arc<Notify>) {
        self.active_connections.fetch_add(1, Relaxed);
        if let Err(err) = self.clone().read_buffer(stream, stop_word.clone()).await {
            println!("[worker {:?}] read_buffer error {:?}", std::thread::current().id(), err);
        }
        self.active_connections.fetch_sub(1, Relaxed);
    }
    async fn spawn_connection(self, pool: Arc<Pool>, socket: TcpStream, stop_word: Arc<Notify>, addr: SocketAddr) -> Result<(), std::io::Error>{
        println!("New connection from {}", addr);
        pool.spawn(async move { self.handle_connection(socket, stop_word.clone()).await }).await
    }
    async fn get_queue(&self, queue_name: &String) -> Result<Arc<Mutex<Queue>>, std::io::Error> {
        let hash = self.queue_names.read().await.get(queue_name).cloned();
        match hash {
            Some(hash) => {
                let queue = self.queue.read().await.get(&hash).cloned();
                match queue {
                    Some(queue) => {
                        Ok(queue.clone()) },
                    None => {
                        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "Queue not found")) },
                }
            }
            None => {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "Hash not found"))
            }
        }
    }
    async fn send_message(self, stream: Arc<Mutex<OwnedWriteHalf>>, queue_name: String, client_id: u128) -> Result<(), std::io::Error> {
        match self.get_queue(&queue_name).await {
            Ok(queue) => {
                let message = queue.lock().await.lock_to_read(client_id);
                match message {
                    Ok(message) => {
                        let message = ResponseMessage::new(Response::Succeeded, message);
                        stream.lock().await.write_all(&message.to_u8()).await?;
                        return Ok(())
                    }
                    Err(err) => {
                        println!("[worker {:?}] send_message error {:?}", std::thread::current().id(), err);
                    }
                }
            }
            Err(err) => {
                println!("[worker {:?}] send_message error {:?}", std::thread::current().id(), err);
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Failed to send message"))
    }
    async fn clear_message_sent(self, queue_name: String, client_id: u128, message_id: Option<u128>) -> Result<(), std::io::Error> {
        match self.get_queue(&queue_name).await {
            Ok(queue) => {
                let cleared = queue.lock().await.dequeue(client_id, message_id);
                match cleared {
                    Ok(_) => {
                        return Ok(())
                        }
                    Err(err) => {
                        return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, err.to_string()))
                    }
                }
            },
            Err(err) => {
                println!("[worker {:?}] clear_message_sent error {:?}", std::thread::current().id(), err);
            }
        }
        Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Failed to clear message sent"))
    }
    async fn send_response(self, stream: Arc<Mutex<OwnedWriteHalf>>, response: &ResponseMessage) {
        match stream.lock().await.write_all(&response.to_u8()).await {
            Ok(_) => (),
            Err(err) => {
                println!("[worker {:?}] response error {:?}", std::thread::current().id(), err);
            }
        }
    }
    async fn read_buffer(&mut self, stream: TcpStream, stop_word: Arc<Notify>) -> Result<(), std::io::Error>
    {
        let mut buffer = BytesMut::with_capacity(1024 * 4);
        let (mut stream_read, stream_write) = stream.into_split();
        let stream_write = Arc::new(Mutex::new(stream_write));

        loop {
            tokio::select! {
            _ = stop_word.notified() => { break; },
            result = stream_read.read_buf(&mut buffer) => {
                if result? == 0 { break; }
            }
        }

            if buffer.len() >= COMMAND_SIZE + CLIENT_ID_SIZE + PAYLOAD_SIZE {
                let command = Request::from_u8(buffer[0])?;
                let client_id = u128::from_be_bytes(buffer[COMMAND_SIZE..(COMMAND_SIZE+CLIENT_ID_SIZE)].try_into().unwrap());
                let payload_size = u64::from_be_bytes(buffer[(COMMAND_SIZE+CLIENT_ID_SIZE)..(COMMAND_SIZE+CLIENT_ID_SIZE+PAYLOAD_SIZE)].try_into().unwrap());
                if buffer.len() < payload_size as usize + (COMMAND_SIZE+CLIENT_ID_SIZE+PAYLOAD_SIZE+QUEUE_NAME_SIZE) {
                    continue;
                }
                let queue_name = String::from_utf8(buffer[(COMMAND_SIZE+CLIENT_ID_SIZE+PAYLOAD_SIZE)..(COMMAND_SIZE+CLIENT_ID_SIZE+PAYLOAD_SIZE+QUEUE_NAME_SIZE)].try_into().unwrap())
                    .map(|s| s.trim_end_matches('\0').to_string());
                let message = RequestMessage::new(&command, buffer.split_to((COMMAND_SIZE+CLIENT_ID_SIZE+PAYLOAD_SIZE+QUEUE_NAME_SIZE) + payload_size as usize));

                let queue_name = queue_name.unwrap_or_else(|err| {
                    println!("[worker {:?}] queue name error {:?}", std::thread::current().id(), err);
                    "".to_string()
                });

                let writer = stream_write.clone();

                match command {
                    Request::Enqueue => {
                        let server = self.clone();
                        tokio::spawn(async move {
                            let message = match message
                            {
                                Ok(message) => message,
                                Err(err) => {
                                    let response = ResponseMessage::new(Response::Failed, vec!());
                                    server.send_response(writer, &response).await;
                                    println!("[worker {:?}] parse_message error {:?}", std::thread::current().id(), err);
                                    return;
                                }
                            };
                            if let Some(queue) = server.queue.read().await.get(&queue_name) {
                                match queue.lock().await.enqueue(message.payload, client_id) {
                                    Ok(_) => {
                                        let response = ResponseMessage::new(Response::Succeeded, vec!());
                                        server.clone().send_response(writer, &response).await;
                                    }
                                    Err(err) => {
                                        let response = ResponseMessage::new(Response::Failed, vec!());
                                        server.clone().send_response(writer, &response).await;
                                        println!("[worker {:?}] enqueue error {:?}", std::thread::current().id(), err);
                                    }
                                }
                            }
                        });
                    }
                    Request::Dequeue => {
                        let server = self.clone();
                        tokio::spawn(async move {
                            match server.clone().send_message(writer.clone(), queue_name, client_id).await {
                                Ok(_) => {},
                                Err(err) => {
                                    let response = ResponseMessage::new(Response::Failed, vec!());
                                    server.clone().send_response(writer, &response).await;
                                    println!("[worker {:?}] send_message error {:?}", std::thread::current().id(), err);
                                }
                            }
                        });
                    }
                    Request::CreateQ => {
                        let server = self.clone();
                        if self.queue.read().await.contains_key(&queue_name) {
                            tokio::spawn(async move {
                                let response = ResponseMessage::new(Response::Failed, vec!());
                                server.send_response(writer, &response).await;
                                println!("Queue already created!");
                            });
                        }
                        else
                        {
                            self.queue.write().await.insert(queue_name, Arc::new(Mutex::new(Queue::new())));
                            tokio::spawn(async move {
                                let response = ResponseMessage::new(Response::Succeeded, vec!());
                                server.send_response(writer, &response).await;
                            });
                        }
                    }
                    Request::DeleteQ => {
                        let server = self.clone();
                        if self.queue.read().await.contains_key(&queue_name) {
                            self.queue.write().await.remove(&queue_name);
                            tokio::spawn(async move {
                                let response = ResponseMessage::new(Response::Succeeded, vec!());
                                server.send_response(writer, &response).await;
                            });
                        }
                        else {
                            tokio::spawn(async move {
                                let response = ResponseMessage::new(Response::Failed, vec!());
                                server.send_response(writer, &response).await;
                                println!("Queue does not exist!");
                            });
                        }
                    }
                    Request::ListM => {
                        let server = self.clone();
                        tokio::spawn(async move {
                            if server.queue.read().await.contains_key(&queue_name) {
                                let list = server.queue.read().await.get(&queue_name).unwrap().lock().await.list_messages();
                                let list = list.unwrap_or_else(|err| {
                                    println!("[worker {:?}] {}", std::thread::current().id(), err);
                                    vec!()
                                });
                                let mut result: Vec<u8> = vec!();
                                for message in list {
                                    result.append(&mut message.to_be_bytes());
                                }
                                {
                                    let response = ResponseMessage::new(Response::Succeeded, result);
                                    server.send_response(writer, &response).await;
                                }
                            }
                        });
                    }
                    Request::DeleteM => {
                        let server = self.clone();
                        tokio::spawn(async move
                        {
                            let message = match message
                            {
                                Ok(message) => message,
                                Err(err) => {
                                    let response = ResponseMessage::new(Response::Failed, vec!());
                                    server.send_response(writer, &response).await;
                                    println!("[worker {:?}] parse_message error {:?}", std::thread::current().id(), err);
                                    return;
                                }
                            };
                            if let Some(queue) = server.queue.read().await.get(&queue_name) {
                                if payload_size < 16 {
                                    let response = ResponseMessage::new(Response::Failed, vec!());
                                    server.clone().send_response(writer, &response).await;
                                    println!("Payload doesn't contain message_id!");
                                    return;
                                }
                                let bytes: [u8; 16] = message.payload.as_slice().try_into().unwrap();
                                let message_id = u128::from_be_bytes(bytes);
                                match queue.lock().await.dequeue(client_id, Some(message_id)) {
                                    Ok(_) => {
                                        let response = ResponseMessage::new(Response::Succeeded, vec!());
                                        server.clone().send_response(writer, &response).await;
                                    }
                                    Err(err) => {
                                        let response = ResponseMessage::new(Response::Failed, vec!());
                                        server.clone().send_response(writer, &response).await;
                                        println!("[worker {:?}] delete message error {:?}", std::thread::current().id(), err);
                                    }
                                }
                            }
                        });
                    }
                    Request::Succeeded => {
                        let server = self.clone();
                        tokio::spawn(async move {
                            if let Some(queue) = server.queue.read().await.get(&queue_name) {
                                match queue.lock().await.dequeue(client_id, None) {
                                    Ok(_) => {
                                        let response = ResponseMessage::new(Response::Succeeded, vec!());
                                        server.clone().send_response(writer, &response).await;
                                    }
                                    Err(err) => {
                                        let response = ResponseMessage::new(Response::Failed, vec!());
                                        server.clone().send_response(writer, &response).await;
                                        println!("[worker {:?}] dequeue message error {:?}", std::thread::current().id(), err);
                                    }
                                }
                            }
                        });
                    }
                    Request::Failed => {
                        let server = self.clone();
                        tokio::spawn(async move {
                           if let Some(queue) = server.queue.read().await.get(&queue_name) {
                               match queue.lock().await.unlock(client_id) {
                                   Ok(_) => {
                                       let response = ResponseMessage::new(Response::Succeeded, vec!());
                                       server.clone().send_response(writer, &response).await;
                                   }
                                   Err(err) => {
                                       let response = ResponseMessage::new(Response::Failed, vec!());
                                       server.clone().send_response(writer, &response).await;
                                       println!("[worker {:?}] unlock message error {:?}", std::thread::current().id(), err);
                                   }
                               }
                           }
                        });
                    }
                    Request::Requeue => {
                        let server = self.clone();
                        tokio::spawn(async move {
                            let message = match message {
                                Ok(message) => message,
                                Err(err) => {
                                    let response = ResponseMessage::new(Response::Failed, vec!());
                                    server.send_response(writer, &response).await;
                                    println!("[worker {:?}] parse_message error {:?}", std::thread::current().id(), err);
                                    return;
                                }
                            };
                            if let Some(queue) = server.queue.read().await.get(&queue_name) {
                                if payload_size < 16 {
                                    let response = ResponseMessage::new(Response::Failed, vec!());
                                    server.clone().send_response(writer, &response).await;
                                    println!("Payload doesn't contain message_id!");
                                    return;
                                }
                                let bytes: [u8; 16] = message.payload.as_slice().try_into().unwrap();
                                let message_id = u128::from_be_bytes(bytes);
                                match queue.lock().await.requeue(client_id, message_id) {
                                    Ok(_) => {
                                        let response = ResponseMessage::new(Response::Succeeded, vec!());
                                        server.clone().send_response(writer, &response).await;
                                    }
                                    Err(err) => {
                                        let response = ResponseMessage::new(Response::Failed, vec!());
                                        server.clone().send_response(writer, &response).await;
                                        println!("[worker {:?}] delete message error {:?}", std::thread::current().id(), err);
                                    }
                                }
                            }
                        });
                    }
                    Request::UpdateM => {
                        let server = self.clone();
                        tokio::spawn(async move {
                            let message = match message {
                                Ok(message) => message,
                                Err(err) => {
                                    let response = ResponseMessage::new(Response::Failed, vec!());
                                    server.send_response(writer, &response).await;
                                    println!("[worker {:?}] parse_message error {:?}", std::thread::current().id(), err);
                                    return;
                                }
                            };
                            if payload_size < 16 {
                                let response = ResponseMessage::new(Response::Failed, vec!());
                                server.clone().send_response(writer, &response).await;
                                println!("Payload doesn't contain message_id!");
                                return;
                            }
                            let bytes: [u8; 16] = message.payload[..16].try_into().unwrap();
                            let message_id = u128::from_be_bytes(bytes);
                            let payload = message.payload[16..].to_vec();
                            if let Some(queue) = server.queue.read().await.get(&queue_name) {
                                match queue.lock().await.update_message(message_id, payload) {
                                    Ok(_) => {
                                        let response = ResponseMessage::new(Response::Succeeded, vec!());
                                        server.clone().send_response(writer, &response).await;
                                    }
                                    Err(err) => {
                                        let response = ResponseMessage::new(Response::Failed, vec!());
                                        server.clone().send_response(writer, &response).await;
                                        println!("[worker {:?}] update message error {:?}", std::thread::current().id(), err);
                                    }
                                }
                            }
                        });
                    }
                }
            }
        }
        Ok(())
    }
}
pub(crate) fn start_server(config: Config, drained: Arc<Notify>) -> Result<(Arc<Notify>, Arc<Notify>), Box<dyn std::error::Error>> {
    let socket_addr = Addr::new(config.server_addr.to_owned(), config.server_port.to_owned());
    let stop_word = Arc::new(Notify::new());
    let stop_accepting = Arc::new(Notify::new());
    let mut server = Server::new(socket_addr?, stop_word.clone(), stop_accepting.clone());
    
    tokio::spawn(async move {
        let result = server.run(config, &drained).await;
        if result.is_err() {
            println!("Server stopped with error: {:?}", result.err().unwrap());
            return;
        }
    });
    Ok((stop_word, stop_accepting))
}
