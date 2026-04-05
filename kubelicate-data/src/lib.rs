pub mod repl;
pub mod traits2;
pub mod types;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
