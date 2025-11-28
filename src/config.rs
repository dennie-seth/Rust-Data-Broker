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
pub(crate) fn parse_config(path: String) -> Result<Config ,Box<dyn std::error::Error>> {
    let mut config = Config { 
        threads_limit: 0, 
        proc_limit: 0,
        wait_limit: 0,
        server_addr: String::from("127.0.0.1"),
        server_port: String::from("8080"),
    };

    if fs::exists(path.clone())? {
        let config_file = fs::read_to_string(path)?;
        let re = Regex::new(r#"(\w+)\s*=\s*(?:"([^"]+)"|(\S+))"#)?;
        let mut data: HashMap<String, String> = HashMap::new();
        for pair in re.captures_iter(&config_file) {
            let key = pair.get(1).unwrap().as_str().to_string();
            let value = pair.get(2).or_else(|| pair.get(3)).unwrap().as_str().to_string();
            data.insert(key, value);
        }
        // region: fill config
        config.threads_limit = data.get("THREADS_LIMIT").unwrap().parse::<u64>().unwrap();
        config.proc_limit = data.get("PROC_LIMIT").unwrap().parse::<u64>().unwrap();
        config.wait_limit = data.get("WAIT_LIMIT").unwrap().parse::<u64>().unwrap();
        config.server_addr = data.get("SERVER_ADDR").unwrap().to_string();
        config.server_port = data.get("SERVER_PORT").unwrap().to_string();
        // endregion
        return Ok(config)
    }
    Err(Box::new(std::io::Error::new(std::io::ErrorKind::NotFound,"Config file not found")))
}
