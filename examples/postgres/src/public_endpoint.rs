use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use super::{PgReplicator, application_error};
use kuberic_runtime::Result;

pub(super) struct PgEndpoint {
    pub(super) cancellation: CancellationToken,
    sockets_cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    sockets: Arc<AtomicUsize>,
    requests: Arc<crate::data_service::RequestTracker>,
    result: Option<std::result::Result<(), String>>,
}

impl Drop for PgEndpoint {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.sockets_cancel.cancel();
    }
}

struct PgSocket {
    stream: tokio::net::TcpStream,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    sockets: Arc<AtomicUsize>,
}

impl Drop for PgSocket {
    fn drop(&mut self) {
        self.sockets.fetch_sub(1, Ordering::AcqRel);
    }
}

impl tonic::transport::server::Connected for PgSocket {
    type ConnectInfo = ();
    fn connect_info(&self) {}
}

impl AsyncRead for PgSocket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "PG endpoint fenced",
            )));
        }
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for PgSocket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.cancelled.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "PG endpoint fenced",
            )));
        }
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }
    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.cancelled.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "PG endpoint fenced",
            )));
        }
        Pin::new(&mut self.stream).poll_flush(context)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

impl PgEndpoint {
    pub(super) async fn bind(
        address: SocketAddr,
        driver: Weak<PgReplicator>,
        token: String,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        if token.is_empty() {
            return Err(application_error(
                "PostgreSQL coordination credentials are required",
            ));
        }
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(application_error)?;
        let sockets = Arc::new(AtomicUsize::new(0));
        let live = sockets.clone();
        let stopped = cancellation.clone();
        let sockets_cancel = CancellationToken::new();
        let accept = sockets_cancel.clone();
        let incoming =
            tokio_stream::wrappers::TcpListenerStream::new(listener).map(move |stream| {
                stream.and_then(|stream| {
                    stream.set_nodelay(true)?;
                    live.fetch_add(1, Ordering::AcqRel);
                    Ok(PgSocket {
                        stream,
                        cancelled: Box::pin(accept.clone().cancelled_owned()),
                        sockets: live.clone(),
                    })
                })
            });
        let requests = Arc::new(crate::data_service::RequestTracker::default());
        let service =
            crate::data_service::PgDataServiceImpl::for_driver(driver, token, requests.clone());
        let task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(service.into_server())
                .serve_with_incoming_shutdown(incoming, stopped.cancelled_owned())
                .await
                .map_err(application_error)
        });
        Ok(Self {
            cancellation,
            sockets_cancel,
            task: Some(task),
            sockets,
            requests,
            result: None,
        })
    }

    pub(super) async fn join(&mut self) -> Result<()> {
        self.cancellation.cancel();
        if self.result.is_none() {
            // Flush terminal RPC replies before fencing idle/pre-auth sockets.
            let task = self.task.as_mut().expect("retained endpoint");
            let joined =
                match tokio::time::timeout(std::time::Duration::from_secs(2), &mut *task).await {
                    Ok(joined) => joined,
                    Err(_) => {
                        self.sockets_cancel.cancel();
                        task.await
                    }
                };
            let result = joined
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            self.task.take();
            self.result = Some(result);
        }
        self.sockets_cancel.cancel();
        let result = self
            .result
            .as_ref()
            .unwrap()
            .clone()
            .map_err(application_error);
        result?;
        self.requests.join().await;
        if self.sockets.load(Ordering::Acquire) != 0 {
            let failure = "PostgreSQL endpoint returned before physical socket settlement";
            self.result = Some(Err(failure.into()));
            return Err(application_error(failure));
        }
        Ok(())
    }
}
