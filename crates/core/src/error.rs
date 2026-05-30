use std::fmt;

#[derive(Debug)]
pub enum UchikomiError {
    ParseError(String),
    GitError(String),
    IoError(String),
    CacheError(String),
    SerializationError(String),
    Other(String),
}

impl fmt::Display for UchikomiError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UchikomiError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            UchikomiError::GitError(msg) => write!(f, "Git error: {}", msg),
            UchikomiError::IoError(msg) => write!(f, "IO error: {}", msg),
            UchikomiError::CacheError(msg) => write!(f, "Cache error: {}", msg),
            UchikomiError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
            UchikomiError::Other(msg) => write!(f, "Error: {}", msg),
        }
    }
}

impl std::error::Error for UchikomiError {}

impl From<anyhow::Error> for UchikomiError {
    fn from(err: anyhow::Error) -> Self {
        UchikomiError::Other(err.to_string())
    }
}

impl From<std::io::Error> for UchikomiError {
    fn from(err: std::io::Error) -> Self {
        UchikomiError::IoError(err.to_string())
    }
}

impl From<git2::Error> for UchikomiError {
    fn from(err: git2::Error) -> Self {
        UchikomiError::GitError(err.to_string())
    }
}
