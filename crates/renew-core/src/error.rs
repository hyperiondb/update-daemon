use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("schedule parse error: {0}")]
    Schedule(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("command `{program}` failed (code {code:?}): {message}")]
    Command {
        program: String,
        code: Option<i32>,
        message: String,
    },

    #[error("git error: {0}")]
    Git(String),

    #[error("docker error: {0}")]
    Docker(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}
