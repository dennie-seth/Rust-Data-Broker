mod net;
mod config;
mod tests;

use std::collections::HashMap;
use tokio::signal;
use crate::config::parse_config;
use crate::net::server::start_server;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Hello, world!");

    // region: config reading
    let env_args: HashMap<String, String> = std::env::args().
        skip(1).
        filter_map(|arg| {
            let mut parts = arg.splitn(2, '=');
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        }).
        collect();
    let mut config_path;
    if env_args.contains_key("--config") {
        config_path = std::path::PathBuf::from(env_args.get("--config").unwrap().to_string());
    }
    else {
        // TODO(bug): parent() returns None at filesystem root, causing a panic. Use
        // current_dir() directly (without .parent()) or require --config to be explicit.
        config_path = std::env::current_dir().unwrap().to_path_buf();
    }
    config_path = config_path.join(".settings");
    println!("{:?}", config_path.to_str());

    let config = parse_config(config_path.to_str().unwrap())?;
    // endregion

    // region: server launch
    let stop_word = start_server(config)?;
    // endregion

    // region: main app loop
    loop {
        tokio::select! {
            _ = stop_word.notified() => { break; },
            reason = wait_for_shutdown() => {
                match reason {
                    ShutdownReason::CtrlC => {
                        println!("Shutting down...");
                        stop_word.notify();
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
            }
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
