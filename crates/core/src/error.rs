use std::fmt;

#[derive(Debug)]
pub enum ChurnLensError {
    ParseError(String),
    GitError(String),
    IoError(String),
    CacheError(String),
    SerializationError(String),
    Other(String),
}

impl fmt::Display for ChurnLensError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ChurnLensError::ParseError(msg) => write!(f, "Parse error: {}", msg),
            ChurnLensError::GitError(msg) => write!(f, "Git error: {}", msg),
            ChurnLensError::IoError(msg) => write!(f, "IO error: {}", msg),
            ChurnLensError::CacheError(msg) => write!(f, "Cache error: {}", msg),
            ChurnLensError::SerializationError(msg) => write!(f, "Serialization error: {}", msg),
            ChurnLensError::Other(msg) => write!(f, "Error: {}", msg),
        }
    }
}

impl std::error::Error for ChurnLensError {}

impl From<anyhow::Error> for ChurnLensError {
    fn from(err: anyhow::Error) -> Self {
        ChurnLensError::Other(err.to_string())
    }
}

impl From<std::io::Error> for ChurnLensError {
    fn from(err: std::io::Error) -> Self {
        ChurnLensError::IoError(err.to_string())
    }
}

impl From<git2::Error> for ChurnLensError {
    fn from(err: git2::Error) -> Self {
        ChurnLensError::GitError(err.to_string())
    }
}
