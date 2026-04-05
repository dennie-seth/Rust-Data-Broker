use std::collections::{HashMap};
use std::io::ErrorKind;
use std::sync::Arc;
use tokio::sync::RwLock;

const MAGIC_DRAIN_VEC: usize = 10usize;
const NET_QUEUE_CONFIG_SIZE: usize = 1usize + 1usize + 8usize;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueueMessage {
    payload: Vec<u8>,
    publisher_id: u128,
    timestamp: u64,
    locked_by: Option<u128>,
}
#[derive(Debug, Clone)]
pub(crate) struct MessageMeta {
    pub id: u128,
    pub publisher_id: u128,
    pub timestamp: u64,
    pub locked_by: Option<u128>,
}
#[derive(Debug, Clone)]
pub(crate) struct QueueConfig {
    auto_success: bool,
    success_timeout: u64,
}
#[derive(Debug, Clone)]
pub(crate) struct NetQueueConfig {
    auto_success: Option<bool>,
    success_timeout: Option<u64>,
}
#[derive(Debug, Clone)]
pub(crate) struct Queue {
    order: Vec<u128>,
    queue: HashMap<u128, QueueMessage>,
    locked: HashMap<u128, Arc<RwLock<Vec<u128>>>>,
    next_id: Option<u128>, // next non-locked message
    config: QueueConfig,
}
impl QueueMessage {
    pub fn new(payload: Vec<u8>, publisher_id: u128) -> Self {
        Self {
            payload,
            publisher_id,
            timestamp: std::time::SystemTime::now().
            duration_since(std::time::UNIX_EPOCH).unwrap().
            as_millis() as u64,
            locked_by: None,
        }
    }
    pub fn is_locked(&self) -> bool {
        self.locked_by.is_some()
    }
    pub fn lock(&mut self, id: u128) {
        self.locked_by = Some(id)
    }
    pub fn unlock(&mut self) {
        self.locked_by = None
    }
}
impl PartialEq<u128> for QueueMessage {
    fn eq(&self, client_id: &u128) -> bool {
        self.locked_by.map_or(false, |id| id == *client_id)
    }
}
impl MessageMeta {
    pub fn new(id: u128, publisher_id: u128, timestamp: u64, locked_by: Option<u128>) -> Self {
        Self {
            id,
            publisher_id,
            timestamp,
            locked_by,
        }
    }
    pub fn to_be_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.append(&mut self.id.to_be_bytes().to_vec());
        bytes.append(&mut self.publisher_id.to_be_bytes().to_vec());
        bytes.append(&mut self.timestamp.to_be_bytes().to_vec());
        bytes.append(&mut self.locked_by.map_or(u128::MAX, |id| id).to_be_bytes().to_vec());
        bytes
    }
}
impl QueueConfig {
    pub fn new() -> Self {
        Self {
            auto_success: false,
            success_timeout: 0,
        }
    }
}
impl NetQueueConfig {
    pub fn new(auto_success: Option<bool>, success_timeout: Option<u64>) -> Self {
        Self {
            auto_success,
            success_timeout,
        }
    }
    pub fn auto_success(&self) -> Option<bool> {
        self.auto_success
    }
    pub fn success_timeout(&self) -> Option<u64> {
        self.success_timeout
    }
    pub fn to_be_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(NET_QUEUE_CONFIG_SIZE);
        let mut flags = 0u8;
        if self.auto_success.is_some() { flags |= 0b01; }
        if self.success_timeout.is_some() { flags |= 0b10; }
        bytes.push(flags);
        if let Some(auto_success) = self.auto_success { bytes.push(auto_success as u8); }
        if let Some(success_timeout) = self.success_timeout { bytes.extend_from_slice(&success_timeout.to_be_bytes()); }
        bytes
    }
    pub fn from_be_bytes(bytes: Vec<u8>) -> Result<(Self, usize), std::io::Error> {
        if bytes.is_empty() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "empty bytes"));
        }
        let flags = bytes[0];
        let mut offset = 1;
        let auto_success = if flags & 0b01 != 0 {
            if bytes.len() < 1 + offset {
                return Err(std::io::Error::new(ErrorKind::InvalidData, "invalid bytes auto_success"));
            }
            let result = bytes[offset] != 0;
            offset += 1;
            Some(result)
        }
        else {
            None
        };
        let success_timeout = if flags & 0b10 != 0 {
            if bytes.len() < 8 + offset {
                return Err(std::io::Error::new(ErrorKind::InvalidData, "invalid bytes success_timeout"));
            }
            let result = u64::from_be_bytes(bytes[offset..offset + 8].try_into().unwrap());
            offset += 8;
            Some(result)
        }
        else {
            None
        };
        Ok((Self {
            auto_success,
            success_timeout,
        }, offset))
    }
}
impl Queue {
    pub fn new() -> Self {
        Self {
            order: vec!(),
            queue: HashMap::new(),
            locked: HashMap::new(),
            next_id: None,
            config: QueueConfig::new(),
        }
    }
    pub fn get_config_auto_success(&self) -> bool {
        self.config.auto_success
    }
    pub fn get_config_success_timeout(&self) -> u64 {
        self.config.success_timeout
    }
    pub fn update_config_auto_success(&mut self, value: bool) {
        self.config.auto_success = value;
    }
    pub fn update_config_success_timeout(&mut self, value: u64) {
        self.config.success_timeout = value;
    }
    pub fn enqueue(&mut self, payload: Vec<u8>, publisher_id: u128) -> Result<(), std::io::Error> {
        let mut id;
        if self.order.is_empty() {
            id = 1;
        }
        else {
            let (result, _) = self.order.last().unwrap().overflowing_add(1);
            // TODO(note): On u128 overflow, result wraps to 0. If messages with low IDs are still
            //            in the queue, this produces a duplicate ID and silently overwrites the
            //            existing entry in self.queue. This is expected behavior - if the message 
            //            stays in queue for so long, it should be deleted by design.
            id = result;
            if id == 0 {
                id = 1;
            }
        }
        if self.next_id.is_none() {
            self.next_id = Some(id);
        }
        self.order.push(id);
        self.queue.insert(id, QueueMessage::new(payload, publisher_id));

        Ok(())
    }
    pub async fn lock_to_read(&mut self, client_id: u128) -> Result<(Vec<u8>, Option<u128>), std::io::Error> {
        if self.order.is_empty() || self.next_id.is_none() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue is empty"));
        }
        if !self.queue.contains_key(&self.next_id.unwrap()) {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "No such message id"));
        }
        if self.queue[&self.next_id.unwrap()].is_locked() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue message already locked"));
        }

        let mut head= vec!();
        let message_id = self.next_id;
        if let Some(message) = self.queue.get_mut(&self.next_id.unwrap()) {
            message.lock(client_id);
            // TODO(bug): `self.locked` is keyed on client_id, so a second Dequeue by the same
            //            client (before acking the first) overwrites the map entry. The first
            //            message still has `locked_by = Some(client_id)` on the `QueueMessage`
            //            itself, so next_id walks past it, but the client can no longer release
            //            it — Succeeded(client_id)/Failed(client_id) operate on the second
            //            lock only, and the first message becomes orphan-locked forever
            //            (can't be re-read, can't be dequeued, can't be requeued by id without
            //            knowing it and then only via the explicit-id DeleteM path). Either
            //            reject lock_to_read when `self.locked.contains_key(&client_id)` or
            //            widen `self.locked` to a `HashMap<u128, HashSet<u128>>`.
            let message_ids = if self.locked.contains_key(&client_id) {
                self.locked.get_mut(&client_id).unwrap().clone()
            }
            else {
                Arc::new(RwLock::new(Vec::<u128>::new()))
            };
            message_ids.clone().write().await.push(self.next_id.unwrap());
            self.locked.insert(client_id, message_ids);
            head = get_meta_as_vec(self.next_id.unwrap(), &message);
        }

        head.append(&mut self.queue[&self.next_id.unwrap()].payload.clone());
        let payload: Vec<u8> = head;

        let mut iter = self.order.iter();
        let _ = iter.find(|&&i| i == self.next_id.unwrap());
        match Some(iter.next()) {
            Some(id) => {
                self.next_id = id.copied();
                if self.next_id == Some(0) {
                    self.next_id = self.order.iter().find(|&&x| x != 0).copied();
                }
            },
            None => {
                self.next_id = None;
            }
        }

        if self.next_id == None {
            println!("End of queue");
            self.remove_zeroes();
        }
        Ok((payload, message_id))
    }
    pub async fn dequeue(&mut self, client_id: u128, message_id: Option<u128>) -> Result<(), std::io::Error> {
        if self.order.is_empty() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue is empty"));
        }
        // Any client can dequeue any message if they know (or guess) its ID. Yes, this is a valid behavior.
        if message_id.is_some() && self.queue.contains_key(&message_id.unwrap()) {
            // Set order id to 0 for now.
            {
                let mut iter = self.order.iter();
                if let Some(id) = iter.position(|&i| i == message_id.unwrap()) {
                    if self.next_id == Some(self.order[id]) {
                        if let Some(next_id) = iter.next() {
                            self.next_id = Some(*next_id);
                        }
                        else
                        {
                            self.next_id = None;
                        }
                    }
                    self.order[id] = 0; // Will clear out all the zeroes in the row later on.
                    if self.next_id == Some(0) {
                        self.next_id = self.order.iter().find(|&&x| x != 0).copied();
                    }
                }
            }
            self.queue.remove(&message_id.unwrap());
            if self.locked.get(&client_id).unwrap().read().await.contains(&message_id.unwrap()) {
                self.locked.remove(&client_id);
                self.remove_zeroes();
            }
            return Ok(());
        }
        if let Some(ids) = self.locked.clone().get(&client_id) {
            if let Some(id) = ids.read().await.clone().into_iter().next() {
                self.queue.remove(&id);
                self.locked.remove(&client_id);
                let mut iter = self.order.iter();
                if let Some(id) = iter.position(|&i| i == id) {
                    self.order[id] = 0;
                }
            }
            else {
                return Err(std::io::Error::new(ErrorKind::InvalidData, "No such message id locked"));
            }
        }
        else {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue message is not locked by client {client_id}"));
        }

        self.remove_zeroes();
        Ok(())
    }
    pub async fn unlock(&mut self, client_id: u128) -> Result<(), std::io::Error> {
        if self.order.is_empty() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue is empty"));
        }
        if !self.locked.contains_key(&client_id) {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue message is not locked by client {client_id}"));
        }
        if let Some(ids) = self.locked.get_mut(&client_id) {
            if let Some(id) = ids.read().await.first()
            {
                ids.write().await.remove(0);
                if let Some(message) = self.queue.get_mut(&id) {
                    message.unlock();
                }
                if self.next_id != None {
                    if id < &self.next_id.unwrap() {
                        self.next_id = Some(*id);
                    }
                } else {
                    self.next_id = Some(*id);
                }
            }
        }
        Ok(())
    }
    pub async fn requeue(&mut self, client_id: u128, message_id: u128) -> Result<(), std::io::Error> {
        if self.order.is_empty() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue is empty"));
        }
        if !self.queue.contains_key(&message_id) {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Message is not in queue"));
        }
        match self.queue.get(&message_id) {
            Some(message) => {
                let payload = message.payload.to_owned();
                let publisher_id = message.publisher_id;

                self.enqueue(payload, publisher_id)?;
                self.dequeue(client_id, Some(message_id)).await?;
            }
            None => {
                return Err(std::io::Error::new(ErrorKind::InvalidData, "Message is not in queue"));
            }
        }
        Ok(())
    }
    pub fn list_messages(&self) -> Result<Vec<MessageMeta>, std::io::Error> {
        let mut result = Vec::<MessageMeta>::with_capacity(self.queue.len());
        for (key, value) in self.queue.iter() {
            result.push(MessageMeta::new(
                *key,
                value.publisher_id,
                value.timestamp,
                value.locked_by
            ))
        }
        Ok(result)
    }
    pub fn update_message(&mut self, message_id: u128, payload: Vec<u8>) -> Result<(), std::io::Error> {
        if self.order.is_empty() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue is empty"));
        }
        if !self.queue.contains_key(&message_id) {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Message is not in queue"));
        }
        match self.queue.get_mut(&message_id) {
            Some(message) => {
                message.payload = payload;
            }
            None => {
                return Err(std::io::Error::new(ErrorKind::InvalidData, "Message is not in queue"));
            }
        }
        Ok(())
    }
    fn remove_zeroes(&mut self) {
        if self.order.len() >= MAGIC_DRAIN_VEC {
            let mut iter = self.order.iter();
            if let Some(id) = iter.position(|&i| i != 0) {
                if id >= (MAGIC_DRAIN_VEC - 1) {
                    self.order.drain(..id);
                }
            }
            else {
                self.order.drain(..);
            }
        }
    }
}
fn get_meta_as_vec(message_id: u128, message: &QueueMessage) -> Vec<u8> {
    MessageMeta::new(
        message_id,
        message.publisher_id,
        message.timestamp,
        message.locked_by,
    ).to_be_bytes().to_vec()
}
