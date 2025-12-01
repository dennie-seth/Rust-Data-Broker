mod net;
mod config;
mod tests;

use std::io::BufRead;
use std::thread;
use std::thread::sleep;
use std::time::Duration;
use tokio::signal;
use crate::config::parse_config;
use crate::net::server::start_server;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Hello, world!");
    
    // region: config reading
    let mut config_path= std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    config_path = config_path.join(".settings");
    println!("{:?}", config_path.to_str());

    let config = parse_config(config_path.to_str().unwrap().to_string())?;
    // endregion
    
    // region: server launch
    let (_, stop_word) = start_server(config)?;
    thread::spawn(move || {
        let mut lock = std::io::stdin().lock();
        loop {
            let mut line = String::new();
            if lock.read_line(&mut line).is_err() { break; }
        }
    });
    // endregion
    
    // region: main app loop
    loop {
        let thread_stop_word = stop_word.clone();
        tokio::spawn(async move {
            let reason = wait_for_shutdown().await;
            match reason {
                ShutdownReason::CtrlC => {
                    println!("Shutting down...");
                    thread_stop_word.notify();
                }
                #[cfg(unix)]
                ShutdownReason::SigTerm => {
                    println!("SigTerm received, shutting down...");
                    stop_word.notify();
                }
                #[cfg(unix)]
                ShutdownReason::SigHup => {
                    println!("SigHup received, not implemented");
                }
            }
        }).await?;
        if stop_word.notified() {
            break;
        }
    }
    // endregion
    Ok(())
}
#[derive(Debug)]
enum ShutdownReason {
    CtrlC,
    #[cfg(unix)]
    SigTerm,
    #[cfg(unix)]
    SigHup,
}
async fn wait_for_shutdown() -> ShutdownReason {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).unwrap();
        let mut sighup = signal(SignalKind::hangup()).unwrap();

        tokio::select! {
            _ = signal::ctrl_c() => {
                return ShutdownReason::CtrlC
            }
            _ = sigterm.recv() => {
                return ShutdownReason::SigTerm
            }
            _ = sighup.recv() => {
                return ShutdownReason::SigHup
            }
        }
    }
    #[cfg(windows)]
    {
        signal::ctrl_c().await.unwrap();
        ShutdownReason::CtrlC
    }
}
