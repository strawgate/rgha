//! Rust client for the GitHub Actions Runner Scale Set API.
//!
//! This is a port of [actions/scaleset](https://github.com/actions/scaleset)
//! (MIT), the protocol library extracted from actions-runner-controller.
//! Flow: authenticate (GitHub App or PAT) → exchange a registration token for
//! an Actions service admin token → create/get a scale set → open a message
//! session → long-poll messages → acquire jobs → mint JIT runner configs.

mod client;
pub mod config;
pub mod listener;
mod session;
pub mod types;

pub use client::{Client, ClientOptions, Credentials, SystemInfo};
pub use listener::{INITIAL_MESSAGE_ID, Listener, Scaler};
pub use session::{HEADER_MAX_CAPACITY, MessageSession};
pub use types::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),
    #[error("{method} {url}: {source}")]
    Transport { method: String, url: String, source: reqwest::Error },
    #[error("HTTP {status} from {url}: {message} (activity id: {activity_id})")]
    Http { status: u16, url: String, activity_id: String, message: String },
    #[error("message queue token expired")]
    QueueTokenExpired,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub(crate) fn build(e: reqwest::Error) -> Self {
        Error::Protocol(format!("building request: {e}"))
    }

    /// The HTTP status, if this error came from a response.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
