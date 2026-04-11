use std::sync::Arc;
use tokio::sync::Mutex;
use crate::errors::ErrorCode;
use crate::net::queue::Queue;

#[derive(Debug, Clone)]
struct StatMessage {
    queue_name: String,
    total_messages: usize,
    total_bytes: usize,
    total_messages_locked: usize,
    total_bytes_locked: usize,
}
#[derive(Debug, Clone)]
pub struct StatWatcher {
    stat_messages: Vec<StatMessage>,
}
impl StatMessage {
    pub fn new(queue_name: String,
           total_messages: usize,
           total_bytes: usize,
           total_messages_locked: usize,
           total_bytes_locked: usize) -> Self {
        Self {
            queue_name,
            total_messages,
            total_bytes,
            total_messages_locked,
            total_bytes_locked,
        }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec!();
        let name = self.queue_name.as_bytes();
        bytes.extend((name.len() as u16).to_be_bytes());
        bytes.extend(name);
        bytes.extend(self.total_messages.to_be_bytes());
        bytes.extend(self.total_bytes.to_be_bytes());
        bytes.extend(self.total_messages_locked.to_be_bytes());
        bytes.extend(self.total_bytes_locked.to_be_bytes());
        bytes
    }
}
impl StatWatcher {
    pub fn new() -> Self {
        Self {
            stat_messages: Vec::new()
        }
    }
    pub async fn new_stat(&mut self,
                    queue_name: String,
                    queue: &Arc<Mutex<Queue>>) -> Result<(), ErrorCode> {
        let lock  = queue.lock().await;
        let total_messages;
        match lock.get_total_messages() {
            Ok(amount) => {
                total_messages = amount;
            }
            Err(err) => {
                println!("Error getting total messages: {}", err);
                return Err(ErrorCode::QueueStatsFailed)
            }
        }
        let total_bytes;
        match lock.get_total_bytes().await {
            Ok(amount) => {
                total_bytes = amount;
            }
            Err(err) => {
                println!("Error getting total bytes: {}", err);
                return Err(ErrorCode::QueueStatsFailed)
            }
        }
        let total_messages_locked;
        match lock.get_total_messages_locked().await {
            Ok(amount) => {
                total_messages_locked = amount;
            }
            Err(err) => {
                println!("Error getting total messages_locked: {}", err);
                return Err(ErrorCode::QueueStatsFailed)
            }
        }
        let total_bytes_locked;
        match lock.get_total_bytes_locked().await {
            Ok(amount) => {
                total_bytes_locked = amount;
            }
            Err(err) => {
                println!("Error getting total bytes_locked: {}", err);
                return Err(ErrorCode::QueueStatsFailed)
            }
        }

        self.stat_messages.push(StatMessage::new(queue_name, total_messages, total_bytes, total_messages_locked, total_bytes_locked));
        Ok(())
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec!();
        bytes.extend((self.stat_messages.len() as u32).to_be_bytes());
        for message in self.stat_messages.iter() {
            let message_bytes = message.to_bytes();
            bytes.extend((message_bytes.len() as u32).to_be_bytes());
            bytes.extend(message_bytes);
        }

        bytes
    }
}
