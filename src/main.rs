mod net;
mod config;
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tokio::time::timeout;
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
    run_shutdown_loop(stop_word, stop_accepting, drained, env_args).await?;
    // endregion
    Ok(())
}

pub(crate) async fn run_shutdown_loop(
    stop_word: Arc<Notify>,
    stop_accepting: Arc<Notify>,
    drained: Arc<Notify>,
    env_args: HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        tokio::select! {
            _ = stop_word.notified() => { break; },
            reason = wait_for_shutdown() => {
                match reason {
                    ShutdownReason::CtrlC => {
                        println!("Shutting down...");
                        shutdown(&stop_accepting, &stop_word, &drained, &env_args).await?;
                        break;
                    }
                    #[cfg(unix)]
                    ShutdownReason::SigTerm => {
                        println!("SigTerm received, gracefully shutting down...");
                        shutdown(&stop_accepting, &stop_word, &drained, &env_args).await?;
                        break;
                    }
                    #[cfg(unix)]
                    ShutdownReason::SigHup => {
                        println!("SigHup received, not implemented");
                    }
                }
            }
        }
    }
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

async fn shutdown(stop_accepting: &Arc<Notify>, stop_word: &Arc<Notify>, drained: &Arc<Notify>, env_args: &HashMap<String, String>) -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}
