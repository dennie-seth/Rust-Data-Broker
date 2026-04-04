use std::collections::{HashMap};
use std::io::ErrorKind;

static MAGIC_DRAIN_VEC: usize = 10usize;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueueMessage {
    payload: Vec<u8>,
    publisher_id: u128,
    timestamp: u64,
    locked_by: Option<u128>,
}
#[derive(Debug, Clone)]
pub(crate) struct Meta {
    pub id: u128,
    pub publisher_id: u128,
    pub timestamp: u64,
    pub locked_by: Option<u128>,
}
#[derive(Debug, Clone)]
pub(crate) struct Queue {
    order: Vec<u128>,
    queue: HashMap<u128, QueueMessage>,
    locked: HashMap<u128, u128>,
    next_id: Option<u128>, // next non-locked message
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
impl Meta {
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
impl Queue {
    pub fn new() -> Self {
        Self {
            order: vec!(),
            queue: HashMap::new(),
            locked: HashMap::new(),
            next_id: None,
        }
    }
    pub fn enqueue(&mut self, payload: Vec<u8>, publisher_id: u128) -> Result<(), std::io::Error> {
        let mut id;
        if self.order.is_empty() {
            id = 1;
        }
        else {
            let (result, _) = self.order.last().unwrap().overflowing_add(1);
            // TODO(bug): On u128 overflow, result wraps to 0. If messages with low IDs are still
            //            in the queue, this produces a duplicate ID and silently overwrites the
            //            existing entry in self.queue. This is expected behavior for now.
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
    pub fn lock_to_read(&mut self, client_id: u128) -> Result<Vec<u8>, std::io::Error> {
        if self.order.is_empty() || self.next_id.is_none() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue is empty"));
        }
        if self.order.binary_search(&self.next_id.unwrap()).is_err() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue message does not exist"));
        }
        if self.queue[&self.next_id.unwrap()].is_locked() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue message already locked"));
        }

        let mut head= vec!();
        if let Some(message) = self.queue.get_mut(&self.next_id.unwrap()) {
            message.lock(client_id);
            self.locked.insert(client_id, self.next_id.unwrap());
            head = get_meta_as_vec(self.next_id.unwrap(), &message);
        }

        head.append(&mut self.queue[&self.next_id.unwrap()].payload.clone());
        let payload: Vec<u8> = head;

        let mut iter = self.order.iter();
        let _ = iter.find(|&&i| i == self.next_id.unwrap());
        match Some(iter.next()) {
            Some(id) => {
                self.next_id = id.copied();
            },
            None => {
                self.next_id = None;
            }
        }

        if self.next_id == None {
            println!("End of queue");
            self.remove_zeroes();
        }
        Ok(payload)
    }
    pub fn dequeue(&mut self, client_id: u128, message_id: Option<u128>) -> Result<(), std::io::Error> {
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
                }
            }
            self.queue.remove(&message_id.unwrap());
            self.locked.remove(&client_id);
            self.remove_zeroes();
            return Ok(());
        }

        if let Some(id) = self.locked.clone().get(&client_id) {
            self.queue.remove(id);
            self.locked.remove(&client_id);
            let mut iter = self.order.iter();
            if let Some(id) = iter.position(|&i| i == *id) {
                self.order[id] = 0;
            }
        }
        else {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue message is not locked by client {client_id}"));
        }

        self.remove_zeroes();
        Ok(())
    }
    pub fn unlock(&mut self, client_id: u128) -> Result<(), std::io::Error> {
        if self.order.is_empty() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue is empty"));
        }
        if !self.locked.contains_key(&client_id) {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue message is not locked by client {client_id}"));
        }
        if let Some(id) = self.locked.remove(&client_id) {
            if let Some(message) = self.queue.get_mut(&id) {
                message.unlock();
            }
            if self.next_id != None {
                if id < self.next_id.unwrap() {
                    self.next_id = Some(id);
                }
            }
            else {
                self.next_id = Some(id);
            }
        }
        Ok(())
    }
    pub fn requeue(&mut self, client_id: u128, message_id: u128) -> Result<(), std::io::Error> {
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
                self.dequeue(client_id, Some(message_id))?;
            }
            None => {
                return Err(std::io::Error::new(ErrorKind::InvalidData, "Message is not in queue"));
            }
        }
        Ok(())
    }
    pub fn list_messages(&self) -> Result<Vec<Meta>, std::io::Error> {
        let mut result = Vec::<Meta>::with_capacity(self.queue.len());
        for (key, value) in self.queue.iter() {
            result.push(Meta::new(
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
    Meta::new(
        message_id,
        message.publisher_id,
        message.timestamp,
        message.locked_by,
    ).to_be_bytes().to_vec()
}
