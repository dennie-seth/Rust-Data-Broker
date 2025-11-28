mod net;
mod config;
mod tests;

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use crate::config::parse_config;
use crate::net::server::start_server;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Hello, world!");
    
    // region: config reading
    let mut config_path= PathBuf::from(file!()).parent().unwrap().to_path_buf();
    config_path = config_path.join("..\\configs\\.settings");
    let config = parse_config(config_path.to_str().unwrap().to_string())?;
    // endregion
    
    // region: server launch
    let (_, stop_word) = start_server(config)?;
    let (sender, receiver) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut lock = std::io::stdin().lock();
        loop {
            let mut line = String::new();
            if lock.read_line(&mut line).is_err() { break; }
            if sender.send(line).is_err() { break; }
        }
    });
    // endregion
    
    // region: main app loop
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
    // endregion
    Ok(())
}
