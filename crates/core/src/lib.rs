pub mod agent;
pub mod db;
pub mod highlight;
pub mod identity;
pub mod import;
pub mod keys;
pub mod known_hosts;
pub mod local;
pub mod macros;
pub mod monitor;
pub mod osc7;
pub mod paths;
pub mod proxy;
pub mod rdp;
pub mod remote;
pub mod serial;
pub mod sessions;
pub mod settings;
pub mod ssh;
pub mod sshconfig;
pub mod telnet;
pub mod terminal;
pub mod termlog;
pub mod theme;
pub mod transfer;
pub mod tunnel;
pub mod vault;
pub mod x11;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("pty: {0}")]
    Pty(String),
    #[error("wrong master password")]
    WrongPassword,
    #[error("vault is locked")]
    Locked,
    #[error("crypto: {0}")]
    Crypto(String),
}
