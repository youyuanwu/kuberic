use thiserror::Error;

#[derive(Debug, Error)]
pub enum XError {
    #[error("Finalizer error: {0}")]
    FinalizerError(#[source] Box<kube::runtime::finalizer::Error<kube::Error>>),
    #[error("MissingObjectKey: {0}")]
    MissingObjectKey(&'static str),
}
