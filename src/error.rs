use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The file failed a structural or checksum check.
    Corrupt(&'static str),
    UnsupportedVersion(u32),
    InvalidPageSize(u32),
    /// Every frame in the buffer pool is pinned.
    PoolExhausted,
    /// A key/value pair exceeds the largest cell a page can hold.
    TooLarge,
    /// Another handle holds the database file lock.
    Locked,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Corrupt(what) => write!(f, "database file is corrupt: {what}"),
            Error::UnsupportedVersion(v) => write!(f, "unsupported file format version {v}"),
            Error::InvalidPageSize(s) => write!(f, "invalid page size {s}"),
            Error::PoolExhausted => f.write_str("buffer pool exhausted: all pages pinned"),
            Error::TooLarge => f.write_str("key/value pair too large for a page"),
            Error::Locked => f.write_str("database is locked by another handle"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
