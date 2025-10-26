use std::net::{SocketAddrV4, };
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncBufReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

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
    println!("New connection from {}", stream.peer_addr().unwrap());

    let (reader, _) = tokio::io::split(stream);
    let mut buf_reader = tokio::io::BufReader::new(reader);
    loop {
        if stop_word.notified() {
            break;
        }
        if buf_reader.fill_buf().await.is_ok() {
            println!("Data in buffer {:?}", buf_reader.buffer());
        }
    }
}
fn spawn_connection(socket: TcpStream, stop_word: Arc<Notify>)  {
    tokio::spawn(async move { handle_connection(socket, stop_word.clone()).await });
}
impl Server {
    fn new(state: Arc<Mutex<ServerState>>, addr: Addr, stop_word: Arc<Notify>) -> Self {
        Self {
            state,
            addr,
            stop_word,
        }
    }
    pub async fn run(&mut self) {
        let listener = TcpListener::bind(&self.addr.addr).await;
        let state = self.state.clone();
        let stop_word = self.stop_word.clone();
        
        tokio::spawn(async move {
            *state.lock().await = ServerState::Waiting;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                if stop_word.notified() {
                    *state.lock().await = ServerState::Stopped;
                    break;
                }
                
                let accepted = listener.as_ref().unwrap().accept().await;
                match accepted.as_ref() {
                    Ok((_, _)) => {
                        let (stream, _) = accepted.unwrap();
                        *state.lock().await = ServerState::Busy;
                        spawn_connection(stream, stop_word.clone());
                    },
                    Err(ref err) => { println!("accept error = {:?}", err); }
                }
            }
        });        
    }
}

pub fn start_server(addr: String, port: String) -> (Arc<Mutex<ServerState>>, Arc<Notify>) {
    let state = Arc::new(Mutex::new(ServerState::Init));
    let socket_addr = Addr::new(addr, port);
    let stop_word = Arc::new(Notify::new());
    let mut server = Server::new(state.clone(), socket_addr.clone(), stop_word.clone());
    
    tokio::spawn(async move {
        server.run().await;
    });

    (state, stop_word) 
}
