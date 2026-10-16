//! Socket destination routing for repository tests; protocol inputs are untouched.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use futures::future::BoxFuture;

type Route =
    Arc<dyn Fn(String, String) -> BoxFuture<'static, std::io::Result<String>> + Send + Sync>;

fn routes() -> &'static Mutex<BTreeMap<String, Route>> {
    static ROUTES: OnceLock<Mutex<BTreeMap<String, Route>>> = OnceLock::new();
    ROUTES.get_or_init(Mutex::default)
}

/// Owns the socket routing registration for one exact test replica instance.
pub struct TestingTcpRoute {
    instance: String,
}

impl TestingTcpRoute {
    /// Register before starting the host. Unregistered engines connect directly.
    pub fn register(
        instance: String,
        route: impl Fn(String, String) -> BoxFuture<'static, std::io::Result<String>>
        + Send
        + Sync
        + 'static,
    ) -> std::io::Result<Self> {
        let mut routes = routes().lock().unwrap();
        if routes.contains_key(&instance) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "instance already routed",
            ));
        }
        routes.insert(instance.clone(), Arc::new(route));
        Ok(Self { instance })
    }
}

impl Drop for TestingTcpRoute {
    fn drop(&mut self) {
        routes().lock().unwrap().remove(&self.instance);
    }
}

pub(super) async fn route(
    instance: &str,
    destination: &str,
    endpoint: String,
) -> std::io::Result<String> {
    let route = routes().lock().unwrap().get(instance).cloned();
    match route {
        Some(route) => route(destination.to_owned(), endpoint).await,
        None => Ok(endpoint),
    }
}
