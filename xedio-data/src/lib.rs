pub mod repl;
pub mod traits2;
pub mod types;
pub mod utils;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

pub const NAME_XEDIO: &str = "xedio";
pub const NAME_XEDIO_OPERATOR: &str = "xdata-operator";
