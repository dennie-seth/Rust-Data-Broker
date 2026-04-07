/// Error codes returned in Failed responses (status byte = 2).
///
/// Codes are grouped by category:
///   0–99    General / protocol errors
///   100–199 Queue-level errors
///   200–299 Message-level errors
///   300–399 Config / payload parsing errors
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    // ── 0–99: General / protocol ──────────────────────────────────────
    /// Unrecognised command byte in the request header.
    UnknownRequestType          = 0,
    /// `RequestMessage::new()` failed to parse the incoming buffer.
    RequestParseError           = 1,

    // ── 100–199: Queue-level errors ───────────────────────────────────
    /// Queue name does not map to any UUID (get_queue → "Hash not found").
    QueueHashNotFound           = 100,
    /// UUID exists in `queue_names` but not in `queue` map (get_queue → "Queue not found").
    QueueNotFound               = 101,
    /// Attempted to create a queue that already exists (CreateQ).
    QueueAlreadyExists          = 102,
    /// Attempted to delete a queue that does not exist (DeleteQ).
    QueueDoesNotExist           = 103,
    /// Queue is empty — no messages to operate on.
    QueueIsEmpty                = 104,

    // ── 200–299: Message-level errors ─────────────────────────────────
    /// Payload is too short to contain a 16-byte message ID.
    PayloadMissingMessageId     = 200,
    /// The referenced message ID does not exist in the queue.
    NoSuchMessageId             = 201,
    /// The next message is already locked by another client (Dequeue / lock_to_read).
    MessageAlreadyLocked        = 202,
    /// No locked message found for the given client (dequeue without explicit ID).
    NoSuchMessageIdLocked       = 203,
    /// The message is not locked by the requesting client (dequeue / unlock).
    MessageNotLockedByClient    = 204,
    /// Message is not in the queue (requeue / update_message).
    MessageNotInQueue           = 205,

    // ── 300–399: Config / payload parsing errors ──────────────────────
    /// UpdateQ payload is shorter than 1 byte.
    InvalidConfigPayloadSize    = 300,
    /// NetQueueConfig bytes are empty.
    ConfigBytesEmpty            = 301,
    /// NetQueueConfig auto_fail flag set but byte missing.
    ConfigInvalidAutoFail       = 302,
    /// NetQueueConfig fail_timeout flag set but not enough bytes.
    ConfigInvalidFailTimeout    = 303,
}

impl ErrorCode {
    pub fn as_u16(self) -> u16 {
        self as u16
    }

    pub fn from_u16(value: u16) -> Option<Self> {
        match value {
            0   => Some(Self::UnknownRequestType),
            1   => Some(Self::RequestParseError),
            100 => Some(Self::QueueHashNotFound),
            101 => Some(Self::QueueNotFound),
            102 => Some(Self::QueueAlreadyExists),
            103 => Some(Self::QueueDoesNotExist),
            104 => Some(Self::QueueIsEmpty),
            200 => Some(Self::PayloadMissingMessageId),
            201 => Some(Self::NoSuchMessageId),
            202 => Some(Self::MessageAlreadyLocked),
            203 => Some(Self::NoSuchMessageIdLocked),
            204 => Some(Self::MessageNotLockedByClient),
            205 => Some(Self::MessageNotInQueue),
            300 => Some(Self::InvalidConfigPayloadSize),
            301 => Some(Self::ConfigBytesEmpty),
            302 => Some(Self::ConfigInvalidAutoFail),
            303 => Some(Self::ConfigInvalidFailTimeout),
            _   => None,
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRequestType        => write!(f, "Unknown request type"),
            Self::RequestParseError         => write!(f, "Failed to parse request message"),
            Self::QueueHashNotFound         => write!(f, "Hash not found"),
            Self::QueueNotFound             => write!(f, "Queue not found"),
            Self::QueueAlreadyExists        => write!(f, "Queue already created"),
            Self::QueueDoesNotExist         => write!(f, "Queue does not exist"),
            Self::QueueIsEmpty              => write!(f, "Queue is empty"),
            Self::PayloadMissingMessageId   => write!(f, "Payload doesn't contain message_id"),
            Self::NoSuchMessageId           => write!(f, "No such message id"),
            Self::MessageAlreadyLocked      => write!(f, "Queue message already locked"),
            Self::NoSuchMessageIdLocked     => write!(f, "No such message id locked"),
            Self::MessageNotLockedByClient  => write!(f, "Queue message is not locked by client"),
            Self::MessageNotInQueue         => write!(f, "Message is not in queue"),
            Self::InvalidConfigPayloadSize  => write!(f, "Invalid config payload size"),
            Self::ConfigBytesEmpty          => write!(f, "Empty bytes"),
            Self::ConfigInvalidAutoFail     => write!(f, "Invalid bytes auto_success"),
            Self::ConfigInvalidFailTimeout  => write!(f, "Invalid bytes success_timeout"),
        }
    }
}