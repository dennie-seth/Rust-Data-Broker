use std::collections::{HashMap};
use std::io::ErrorKind;

static MAGIC_DRAIN_VEC: usize = 10usize;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueueMessage {
    payload: Vec<u8>,
    publisher_id: u16,
    timestamp: u64,
    locked_by: u16,
}
#[derive(Debug, Clone)]
pub(crate) struct Queue {
    name: String,
    order: Vec<u128>,
    queue: HashMap<u128, QueueMessage>,
    locked: HashMap<u16, u128>,
    next_id: u128, // next non-locked message
}
impl QueueMessage {
    pub fn new(payload: Vec<u8>, publisher_id: u16) -> Self {
        Self {
            payload,
            publisher_id,
            timestamp: std::time::SystemTime::now().
            duration_since(std::time::UNIX_EPOCH).unwrap().
            as_millis() as u64,
            locked_by: 0,
        }
    }
    pub fn is_locked(&self) -> bool {
        self.locked_by != 0
    }
    pub fn lock(&mut self, id: u16) {
        self.locked_by = id
    }
    pub fn unlock(&mut self) {
        self.locked_by = 0;
    }
}
impl PartialEq<u16> for QueueMessage {
    fn eq(&self, client_id: &u16) -> bool {
        self.locked_by == *client_id
    }
}
impl Queue {
    pub fn new(name: String) -> Self {
        Self {
            name,
            order: vec!(),
            queue: HashMap::new(),
            locked: HashMap::new(),
            next_id: 0,
        }
    }
    pub fn enqueue(&mut self, payload: Vec<u8>, publisher_id: u16) {
        let id;
        if self.order.is_empty() {
            id = 0;
        }
        else {
            let (result, _) = self.order.last().unwrap().overflowing_add(1);
            id = result;
        }
        self.order.push(id);
        self.queue.insert(id, QueueMessage::new(payload, publisher_id));
    }
    pub fn lock_to_read(&mut self, client_id: u16) -> Result<Vec<u8>, std::io::Error> {
        if self.order.is_empty() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue is empty"));
        }
        if !self.order.contains(&self.next_id) {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue message does not exist"));
        }
        if self.queue[&self.next_id].is_locked() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue message already locked"));
        }

        if let Some(message) = self.queue.get_mut(&self.next_id) {
            message.lock(client_id);
            self.locked.insert(client_id, self.next_id);
        }

        let payload = self.queue[&self.next_id].payload.clone();

        let mut iter = self.order.iter();
        let _ = iter.find(|&&i| i == self.next_id);
        self.next_id = *iter.next().ok_or_else(|| 0).unwrap();

        if self.next_id == 0 {
            println!("End of queue");
            self.remove_zeroes();
        }
        Ok(payload)
    }
    pub fn dequeue(&mut self, client_id: u16, message_id: Option<u128>) -> Result<(), std::io::Error> {
        if self.order.is_empty() {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "Queue is empty"));
        }
        if message_id.is_some() && self.queue.contains_key(&message_id.unwrap()) {
            // Set order id to 0 for now.
            {
                let mut iter = self.order.iter();
                if let Some(id) = iter.position(|&i| i == message_id.unwrap()) {
                    if self.next_id == self.order[id] {
                        if let Some(next_id) = iter.next() {
                            self.next_id = *next_id;
                        }
                        else
                        {
                            self.next_id = 0;
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
    pub fn unlock(&mut self, client_id: u16) -> Result<(), std::io::Error> {
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
            if id < self.next_id {
                self.next_id = id;
            }
        }
        Ok(())
    }
    fn remove_zeroes(&mut self) {
        if self.order.len() >= MAGIC_DRAIN_VEC {
            let mut iter = self.order.iter();
            if let Some(id) = iter.position(|&i| i != 0) {
                if id >= (MAGIC_DRAIN_VEC - 1) {
                    self.order.drain(..id - 1);
                }
            }
            else {
                self.order.drain(..);
            }
        }
    }
}
