mod net;

use std::io::BufRead;
use std::sync::mpsc;
use std::thread;
use crate::net::server::start_server;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    println!("Hello, world!");
    let (server_state, stop_word) = start_server("127.0.0.1".to_owned(), "8080".to_owned());
    let (sender, receiver) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut lock = std::io::stdin().lock();
        loop {
            let mut line = String::new();
            if lock.read_line(&mut line).is_err() { break; }
            if sender.send(line).is_err() { break; }
        }
    });
    
    loop {
        match receiver.try_recv() {
            Ok(message) => {
                assert_ne!(message.trim(), "", "No input!");
                if message.eq_ignore_ascii_case("q\r\n") {
                    break;
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => { break; }
        }
    }
    stop_word.notify();
}
