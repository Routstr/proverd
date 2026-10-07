//! WebSocket → byte-stream adapter.
//!
//! Framing matches `ws_stream_wasm` (used by the tlsn wasm verifier): every
//! `write()` becomes exactly one binary WebSocket message; reads concatenate
//! the payloads of incoming binary messages. Text/ping/pong are skipped,
//! close ends the stream.

use std::{
    io,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll, ready},
};

use axum::extract::ws::{Message, WebSocket};
use futures::{AsyncRead, AsyncWrite, Sink, Stream};

/// What a decoded ws message means for the byte stream.
pub enum MsgKind {
    /// Binary payload bytes.
    Payload(Vec<u8>),
    /// Stream ends.
    Close,
    /// Control/text frame: no bytes, keep polling.
    Skip,
}

/// Message type abstraction so the same pipe works for axum (server) and
/// tokio-tungstenite (test client) message types.
pub trait WsMsg {
    /// Classify a received message.
    fn kind(self) -> MsgKind;
    /// Build an outgoing binary message.
    fn from_bytes(data: Vec<u8>) -> Self;
}

impl WsMsg for Message {
    fn kind(self) -> MsgKind {
        match self {
            Message::Binary(data) => MsgKind::Payload(data.to_vec()),
            Message::Close(_) => MsgKind::Close,
            _ => MsgKind::Skip,
        }
    }

    fn from_bytes(data: Vec<u8>) -> Self {
        Message::Binary(data.into())
    }
}

/// Split stream/sink pair wrapper presenting a ws connection as an
/// item-stream of byte chunks and a sink of byte chunks, suitable for
/// `async_io_stream::IoStream`.
///
/// The receive half must yield `io::Result<T>` items (map errors first).
pub struct WsPipe<St, Si, T> {
    stream: St,
    sink: Si,
    _msg: PhantomData<fn() -> T>,
}

impl<St, Si, T> WsPipe<St, Si, T> {
    /// Wrap receive/send halves of a ws connection.
    pub fn new(stream: St, sink: Si) -> Self {
        Self {
            stream,
            sink,
            _msg: PhantomData,
        }
    }
}

impl<St, Si, T> Stream for WsPipe<St, Si, T>
where
    St: Stream<Item = io::Result<T>> + Unpin,
    Si: Unpin,
    T: WsMsg,
{
    type Item = io::Result<Vec<u8>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match ready!(Pin::new(&mut self.stream).poll_next(cx)) {
                Some(Ok(msg)) => match msg.kind() {
                    MsgKind::Payload(bytes) if bytes.is_empty() => continue,
                    MsgKind::Payload(bytes) => return Poll::Ready(Some(Ok(bytes))),
                    MsgKind::Close => return Poll::Ready(None),
                    MsgKind::Skip => continue,
                },
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                None => return Poll::Ready(None),
            }
        }
    }
}

impl<St, Si, T> Sink<Vec<u8>> for WsPipe<St, Si, T>
where
    St: Unpin,
    Si: Sink<T> + Unpin,
    Si::Error: std::error::Error + Send + Sync + 'static,
    T: WsMsg,
{
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.sink)
            .poll_ready(cx)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn start_send(mut self: Pin<&mut Self>, item: Vec<u8>) -> io::Result<()> {
        // Never emit empty binary messages: adapters on the other end
        // (wasm JsIoAdapter) treat an empty read as EOF.
        if item.is_empty() {
            return Ok(());
        }
        Pin::new(&mut self.sink)
            .start_send(T::from_bytes(item))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.sink)
            .poll_flush(cx)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.sink)
            .poll_close(cx)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
}

/// Wrap an axum server-side WebSocket as a futures AsyncRead + AsyncWrite
/// byte stream suitable for `tlsn::Session::new`.
pub fn ws_to_io(ws: WebSocket) -> impl AsyncRead + AsyncWrite + Send + Unpin + 'static {
    use futures::StreamExt;
    let (sink, stream) = ws.split();
    let stream = stream.map(|r| {
        r.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    });
    async_io_stream::IoStream::new(WsPipe::new(stream, sink))
}
