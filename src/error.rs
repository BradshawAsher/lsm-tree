use thiserror::Error;

#[derive(Error, Debug)]
pub enum LsmError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("CRC checksum mismatch: expected {expected:#010x}, calculated {actual:#010x}")]
    CrcMismatch { expected: u32, actual: u32 },

    #[error("Invalid operation code in WAL: {0}")]
    InvalidOpCode(u8),

    #[error("Torn write detected in WAL at offset {offset}: {detail}")]
    TornWrite { offset: u64, detail: String },

    #[error("Key exceeds maximum allowed length of {max} bytes (was {actual} bytes)")]
    KeyTooLarge { actual: usize, max: usize },

    #[error("Corrupted SSTable: {0}")]
    CorruptedSSTable(String),
}

pub type Result<T> = std::result::Result<T, LsmError>;
