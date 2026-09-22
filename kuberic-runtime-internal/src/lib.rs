//! Unpublished contracts shared by the level-triggered runtime and replica agent.

pub mod authority;
pub mod effects;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContractError {
    #[error("authority mismatch: {0}")]
    AuthorityMismatch(String),
    #[error("persistence error: {0}")]
    Persistence(String),
}

pub type Result<T> = std::result::Result<T, ContractError>;
