use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadUrl(String),
    #[error("request failed: {0}")]
    Http(String),
    #[error("server answered HTTP {status} for {url}")]
    Status { status: u16, url: String },
    #[error("not found (HTTP 404): {0}")]
    NotFound(String),
    #[error("could not parse response: {0}")]
    Parse(String),
    #[error("cookie file {path}: {msg}")]
    Cookies { path: PathBuf, msg: String },
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("library database: {0}")]
    Db(String),
    #[error("cancelled")]
    Cancelled,
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e.to_string())
    }
}

impl Error {
    /// Convert a reqwest error without leaking the request URL's query/credentials into logs beyond
    /// what reqwest already prints for the failing URL.
    pub fn from_reqwest(e: reqwest::Error) -> Self {
        Error::Http(e.without_url().to_string())
    }
}
