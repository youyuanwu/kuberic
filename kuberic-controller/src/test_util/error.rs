use std::{error::Error, fmt, sync::Arc};

#[derive(Clone, Debug)]
pub(super) struct RetainedFailure(Arc<anyhow::Error>);

impl RetainedFailure {
    pub(super) fn new(source: anyhow::Error) -> Self {
        Self(Arc::new(source))
    }
    fn kind(&self) -> ErrorKind {
        self.0
            .downcast_ref::<TestError>()
            .map_or(ErrorKind::Fatal, TestError::kind)
    }
}
impl fmt::Display for RetainedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl Error for RetainedFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

/// Classification of a failed scenario or harness operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    InvalidConfiguration,
    InvalidReference,
    UnsupportedCapability,
    FaultAlreadyActive,
    Application,
    Deadline,
    Fatal,
    Cleanup,
}

/// Preserves application and cleanup sources without printing credentials.
pub struct TestError {
    kind: ErrorKind,
    message: String,
    sources: Vec<anyhow::Error>,
    diagnostics: Option<serde_json::Value>,
}

pub type Result<T> = std::result::Result<T, TestError>;

impl TestError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            sources: Vec::new(),
            diagnostics: None,
        }
    }
    pub fn with_source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: impl Into<anyhow::Error>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            sources: vec![source.into()],
            diagnostics: None,
        }
    }
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
    pub fn diagnostics(&self) -> Option<&serde_json::Value> {
        self.diagnostics.as_ref()
    }
    pub fn sources(&self) -> impl Iterator<Item = &(dyn Error + Send + Sync + 'static)> {
        self.sources.iter().map(|source| source.as_ref())
    }
    pub(super) fn combined(
        kind: ErrorKind,
        message: impl Into<String>,
        sources: Vec<anyhow::Error>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            sources,
            diagnostics: None,
        }
    }
    pub(super) fn with_diagnostics(mut self, value: serde_json::Value) -> Self {
        self.diagnostics = Some(value);
        self
    }
}

impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}
impl fmt::Debug for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("source_count", &self.sources.len())
            .finish()
    }
}
impl Error for TestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.sources.first().map(|source| source.as_ref() as _)
    }
}
impl From<anyhow::Error> for TestError {
    fn from(source: anyhow::Error) -> Self {
        match source.downcast::<Self>() {
            Ok(error) => error,
            Err(source) => {
                let kind = if let Some(retained) = source.downcast_ref::<RetainedFailure>() {
                    retained.kind()
                } else if source.is::<tokio::time::error::Elapsed>() {
                    ErrorKind::Deadline
                } else {
                    ErrorKind::Fatal
                };
                Self::with_source(kind, "harness operation failed", source)
            }
        }
    }
}
impl From<std::io::Error> for TestError {
    fn from(source: std::io::Error) -> Self {
        Self::with_source(
            ErrorKind::Application,
            "application storage operation failed",
            source,
        )
    }
}
