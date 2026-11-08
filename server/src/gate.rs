//! Admission and deadlines.
//!
//! Signed requests, once their signature is verified (`api::verify`), take a
//! permit: `RUNNING` run at once, and apart from them `RUNNING_LARGE`
//! collection reads and writes (each up to ~21 MiB, with its copies). Each
//! pool has its own queue (`WAITING`, `WAITING_LARGE`) whose requests wait
//! at most `ADMISSION_WAIT` for a permit; anything beyond is told the server
//! is busy (503). So memory stays bounded, small requests never wait behind
//! transfers, and a request without a valid signature holds no permit. The permit travels with the response body, so it is
//! released only once the response has been sent. A collection write must
//! arrive and be answered within `WRITE_TIMEOUT`, any other request within
//! `SMALL_TIMEOUT`, and every response must reach the client within
//! `SEND_TIMEOUT`. `/v2/health` stays outside the gate.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::serve::{IncomingStream, Listener};
use http_body::{Frame, SizeHint};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep};

use crate::api::ApiError;

/// Signed requests handled at once.
pub(crate) const RUNNING: usize = 16;
/// Collection reads and writes handled at once.
pub(crate) const RUNNING_LARGE: usize = 4;
/// Requests that may wait for a permit of each pool; more are refused at
/// once.
const WAITING: usize = 64;
const WAITING_LARGE: usize = 16;
/// How long a request waits for a permit.
const ADMISSION_WAIT: Duration = Duration::from_secs(2);
/// Any request but a collection write: at most 4 KiB, read and answered
/// within this time.
pub(crate) const SMALL_TIMEOUT: Duration = Duration::from_secs(10);
/// A collection write, upload of up to ~21 MiB included.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(120);
/// From a response being ready to its last byte leaving the server.
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(120);

/// A collection write: the one request with a large body.
pub(crate) fn collection_write(method: &Method, path: &str) -> bool {
    method == Method::PUT && path.starts_with("/collections/")
}

pub(crate) struct Gate {
    running: Arc<Semaphore>,
    large: Arc<Semaphore>,
    waiting: Semaphore,
    waiting_large: Semaphore,
}

impl Gate {
    pub(crate) fn new() -> Self {
        Self {
            running: Arc::new(Semaphore::new(RUNNING)),
            large: Arc::new(Semaphore::new(RUNNING_LARGE)),
            waiting: Semaphore::new(WAITING),
            waiting_large: Semaphore::new(WAITING_LARGE),
        }
    }

    async fn enter(&self, large: bool) -> Option<OwnedSemaphorePermit> {
        let (pool, queue) = if large {
            (&self.large, &self.waiting_large)
        } else {
            (&self.running, &self.waiting)
        };
        let _waiting = queue.try_acquire().ok()?;
        // The semaphore is never closed, so acquiring only waits.
        tokio::time::timeout(ADMISSION_WAIT, pool.clone().acquire_owned())
            .await
            .ok()?
            .ok()
    }
}

/// Middleware for signed requests: admission, then the request deadline.
pub(crate) async fn admit(State(gate): State<Arc<Gate>>, req: Request, next: Next) -> Response {
    let large = req.uri().path().starts_with("/collections/");
    let Some(permit) = gate.enter(large).await else {
        return ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the server is busy, try again in a moment",
        )
        .into_response();
    };
    let limit = if collection_write(req.method(), req.uri().path()) {
        WRITE_TIMEOUT
    } else {
        SMALL_TIMEOUT
    };
    tokio::time::timeout(limit, next.run(req))
        .await
        .unwrap_or_else(|_| {
            ApiError::new(StatusCode::REQUEST_TIMEOUT, "request took too long").into_response()
        })
        .map(|body| {
            Body::new(Permitted {
                body,
                _permit: permit,
            })
        })
}

/// A response body that holds its request's permit until hyper has sent it
/// or given up on it (both drop the body).
struct Permitted {
    body: Body,
    _permit: OwnedSemaphorePermit,
}

impl HttpBody for Permitted {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.get_mut().body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// When the response a connection is sending must be out. The router sets
/// it as each response leaves (`send_deadline`), the connection enforces it
/// (`Conn`): hyper itself has no write timeout, and a client that reads
/// slowly would otherwise keep a large response, and its permit, forever.
#[derive(Clone, Default)]
pub(crate) struct SendDeadline(Arc<Mutex<Option<Instant>>>);

impl SendDeadline {
    fn set(&self, at: Option<Instant>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = at;
    }

    fn get(&self) -> Option<Instant> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Connected<IncomingStream<'_, Accept>> for SendDeadline {
    fn connect_info(stream: IncomingStream<'_, Accept>) -> Self {
        stream.io().deadline.clone()
    }
}

/// Middleware for every route: a response gets `SEND_TIMEOUT` from when it
/// is ready. Without a connection (tests calling the router) it does nothing.
pub(crate) async fn send_deadline(req: Request, next: Next) -> Response {
    let deadline = req
        .extensions()
        .get::<ConnectInfo<SendDeadline>>()
        .map(|c| c.0.clone());
    // Writes while the request is read (`100 Continue`) are not limited.
    if let Some(d) = &deadline {
        d.set(None);
    }
    let res = next.run(req).await;
    if let Some(d) = deadline {
        d.set(Some(Instant::now() + SEND_TIMEOUT));
    }
    res
}

/// A TCP listener whose connections enforce `SendDeadline`.
pub(crate) struct Accept(TcpListener);

impl Accept {
    pub(crate) async fn bind(addr: SocketAddr) -> io::Result<Self> {
        TcpListener::bind(addr).await.map(Self)
    }
}

impl Listener for Accept {
    type Io = Conn;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Conn, SocketAddr) {
        // axum's own accept loop, which retries after errors.
        let (tcp, addr) = Listener::accept(&mut self.0).await;
        (Conn::new(tcp), addr)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

/// A connection whose writes fail once its `SendDeadline` has passed; hyper
/// then closes it and drops the response. hyper also flushes an idle
/// connection, so a keep-alive connection is closed when it stays idle that
/// long after its last response.
pub(crate) struct Conn {
    tcp: TcpStream,
    deadline: SendDeadline,
    timer: Option<Pin<Box<Sleep>>>,
}

impl Conn {
    fn new(tcp: TcpStream) -> Self {
        Self {
            tcp,
            deadline: SendDeadline::default(),
            timer: None,
        }
    }

    /// Fails once the deadline has passed. Until then the timer wakes the
    /// connection at the deadline, so a write stalled on a client that does
    /// not read notices it too.
    fn on_time(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        let Some(at) = self.deadline.get() else {
            return Ok(());
        };
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(at)));
        if timer.deadline() != at {
            timer.as_mut().reset(at);
        }
        match timer.as_mut().poll(cx) {
            Poll::Ready(()) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the client did not take the response in time",
            )),
            Poll::Pending => Ok(()),
        }
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().tcp).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.on_time(cx)?;
        Pin::new(&mut this.tcp).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.on_time(cx)?;
        Pin::new(&mut this.tcp).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.tcp.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.on_time(cx)?;
        Pin::new(&mut this.tcp).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().tcp).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test(start_paused = true)]
    async fn a_response_the_client_does_not_read_is_cut_off() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let mut conn = Conn::new(listener.accept().await.unwrap().0);
        let start = Instant::now();
        conn.deadline.set(Some(start + SEND_TIMEOUT));
        // Far more than the socket buffers hold; the client never reads.
        let err = conn
            .write_all(&vec![0u8; 64 * 1024 * 1024])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() >= SEND_TIMEOUT);
        drop(client);
    }
}
