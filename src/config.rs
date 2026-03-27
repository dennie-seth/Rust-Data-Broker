use std::collections::HashMap;
use std::fs;
use std::sync::OnceLock;
use regex::Regex;

pub struct Config
{
    pub threads_limit: u64,
    pub proc_limit: u64,
    pub wait_limit: u64,
    pub server_addr: String,
    pub server_port: String,
}
static RE: OnceLock<Regex> = OnceLock::new();
pub(crate) fn parse_config(path: &str) -> Result<Config ,Box<dyn std::error::Error>> {
    let config_file = fs::read_to_string(path);
    match config_file {
        Ok(_) => {
            let re = RE.get_or_init(|| Regex::new(r#"(\w+)\s*=\s*(?:"([^"]+)"|(\S+))"#).unwrap());
            let mut data: HashMap<String, String> = HashMap::new();

            for pair in re.captures_iter(&config_file.unwrap()) {
                let key = pair.get(1).unwrap().as_str().to_string();
                let value = pair.get(2).or_else(|| pair.get(3)).unwrap().as_str().to_string();
                data.insert(key, value);
            }
            // region: fill config
            let config: Config = Config {
                threads_limit: data.get("THREADS_LIMIT").ok_or("missing THREADS_LIMIT")?.parse::<u64>()?,
                // TODO(design): PROC_LIMIT is parsed and stored in Config but never read anywhere in
                // server.rs. Either use it (e.g. as a per-worker job concurrency cap) or remove it
                // from both the Config struct and the .settings file to avoid confusion.
                proc_limit : data.get("PROC_LIMIT").ok_or("missing PROC_LIMIT") ?.parse:: < u64>()?,
                wait_limit : data.get("WAIT_LIMIT").ok_or("missing WAIT_LIMIT") ?.parse:: < u64>()?,
                server_addr : data.get("SERVER_ADDR").ok_or("missing SERVER_ADDR") ?.to_string(),
                server_port : data.get("SERVER_PORT").ok_or("missing SERVER_PORT")?.to_string(),
            };
            // endregion
            Ok(config)
        },
        Err(err) => {
            Err(Box::new(err))
        }
    }
}
