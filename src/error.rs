use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UrdError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Chain error: {0}")]
    Chain(String),

    #[error("Retention error: {0}")]
    #[allow(dead_code)]
    Retention(String),

    #[error("Btrfs command failed: {0}")]
    Btrfs(String),

    #[error("State database error: {0}")]
    State(String),
}

pub type Result<T> = std::result::Result<T, UrdError>;
