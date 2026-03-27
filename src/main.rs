mod net;
mod config;
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::signal;
use crate::config::parse_config;
use crate::net::server::{start_server, Notify};

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
        config_path = std::env::current_dir().unwrap().to_path_buf();
    }
    config_path = config_path.join(".settings");
    println!("{:?}", config_path.to_str());

    let config = parse_config(config_path.to_str().unwrap())?;
    // endregion

    // region: server launch
    let drained = Arc::new(Notify::new());
    let (stop_word, stop_accepting) = start_server(config, drained.clone())?;
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
                        stop_accepting.notify();
                    }
                    #[cfg(unix)]
                    ShutdownReason::SigTerm => {
                        println!("SigTerm received, gracefully shutting down...");
                        stop_accepting.notify();
                        if env_args.contains_key("--shutdown_timeout") {
                            let mut duration = env_args.get("--shutdown_timeout").unwrap().parse::<u64>()?;
                            if duration == 0 {
                                duration = 30;
                            }
                            timeout(Duration::from_secs(duration), drained.notified()).await.
                                        unwrap_or_else(|_| {
                                            println!("Shutdown timed out!");
                                            stop_word.notify();
                                    });
                            stop_word.notify();
                        }
                        else {
                            // No timeout specified — stop immediately without waiting for drain.
                            stop_word.notify();
                        }
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
