use std::collections::HashMap;
use std::fs;
use regex::Regex;

pub struct Config
{
    pub threads_limit: u64,
    pub proc_limit: u64,
    pub wait_limit: u64,
    pub server_addr: String,
    pub server_port: String,
}
// TODO(suggestion): `path` is taken as an owned `String` but ownership is never needed.
// Prefer `&str` or `&std::path::Path` to avoid unnecessary allocations at the call site.
pub(crate) fn parse_config(path: &str) -> Result<Config ,Box<dyn std::error::Error>> {
    let mut config = Config { 
        threads_limit: 0, 
        proc_limit: 0,
        wait_limit: 0,
        server_addr: String::from("127.0.0.1"),
        server_port: String::from("8080"),
    };

    if fs::exists(path)? {
        let config_file = fs::read_to_string(path)?;
        let re = Regex::new(r#"(\w+)\s*=\s*(?:"([^"]+)"|(\S+))"#)?;
        let mut data: HashMap<String, String> = HashMap::new();
        for pair in re.captures_iter(&config_file) {
            let key = pair.get(1).unwrap().as_str().to_string();
            let value = pair.get(2).or_else(|| pair.get(3)).unwrap().as_str().to_string();
            data.insert(key, value);
        }
        // region: fill config
        // TODO(bug): Every `.unwrap()` here panics if a key is missing from the file or if the
        // value is not a valid number. The function signature already returns a Result — use it:
        //   config.threads_limit = data.get("THREADS_LIMIT")
        //       .ok_or("missing THREADS_LIMIT")?
        //       .parse::<u64>()
        //       .map_err(|e| format!("invalid THREADS_LIMIT: {e}"))?;
        // Apply the same pattern to all keys below.
        config.threads_limit = data.get("THREADS_LIMIT").ok_or("missing THREADS_LIMIT")?.parse::<u64>()?;
        // TODO(bug): PROC_LIMIT is parsed and stored in Config but never read anywhere in
        // server.rs. Either use it (e.g. as a per-worker job concurrency cap) or remove it
        // from both the Config struct and the .settings file to avoid confusion.
        config.proc_limit = data.get("PROC_LIMIT").ok_or("missing PROC_LIMIT")?.parse::<u64>()?;
        config.wait_limit = data.get("WAIT_LIMIT").ok_or("missing WAIT_LIMIT")?.parse::<u64>()?;
        config.server_addr = data.get("SERVER_ADDR").ok_or("missing SERVER_ADDR")?.to_string();
        config.server_port = data.get("SERVER_PORT").ok_or("missing SERVER_PORT")?.to_string();
        // endregion
        return Ok(config)
    }
    Err(Box::new(std::io::Error::new(std::io::ErrorKind::NotFound,"Config file not found")))
}
