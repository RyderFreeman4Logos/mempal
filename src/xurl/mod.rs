pub mod backfill;
pub mod embed;
pub mod ingest;
pub mod model;
pub mod parser;
pub(crate) mod reconcile;
pub mod search;
pub mod store;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum XurlError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(String),
}

pub type XurlResult<T> = Result<T, XurlError>;
