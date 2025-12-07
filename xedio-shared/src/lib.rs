pub mod kube_util;
pub mod proto;
pub mod storage_client;
pub mod utils;

pub const NAME_XEDIO: &str = "xedio";
pub const NAME_XEDIO_OPERATOR: &str = "xdata-operator";
pub const XEDIO_TEST_NAMESPACE: &str = "xedio-test-ns";

pub type Error = Box<dyn std::error::Error + Send + Sync>;
