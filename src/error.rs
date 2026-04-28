use thiserror::Error;

pub type Result<T> = std::result::Result<T, MnemeError>;

#[derive(Debug, Error)]
pub enum MnemeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("index error: {0}")]
    Index(String),

    #[error("MCP protocol error: {0}")]
    Mcp(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("write-ahead log error: {0}")]
    Wal(String),

    #[error("redb error: {0}")]
    Redb(String),

    #[error("lock error: {0}")]
    Lock(String),

    #[error("schema migration error: {0}")]
    Migration(String),

    #[error("disk full")]
    DiskFull,
}

impl From<redb::Error> for MnemeError {
    fn from(err: redb::Error) -> Self {
        MnemeError::Redb(err.to_string())
    }
}

impl From<redb::DatabaseError> for MnemeError {
    fn from(err: redb::DatabaseError) -> Self {
        MnemeError::Redb(err.to_string())
    }
}

impl From<redb::TransactionError> for MnemeError {
    fn from(err: redb::TransactionError) -> Self {
        MnemeError::Redb(err.to_string())
    }
}

impl From<redb::TableError> for MnemeError {
    fn from(err: redb::TableError) -> Self {
        MnemeError::Redb(err.to_string())
    }
}

impl From<redb::StorageError> for MnemeError {
    fn from(err: redb::StorageError) -> Self {
        MnemeError::Redb(err.to_string())
    }
}

impl From<redb::CommitError> for MnemeError {
    fn from(err: redb::CommitError) -> Self {
        MnemeError::Redb(err.to_string())
    }
}
