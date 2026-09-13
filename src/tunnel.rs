//! Serving on the socket this process dialed, so all traffic is outbound.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tonic::transport::server::Connected;

/// tonic serves only over IO implementing `Connected`, which `tokio_rustls`'
/// stream does not.
pub struct Stream<T>(pub T);

impl<T> Connected for Stream<T> {
    type ConnectInfo = ();
    fn connect_info(&self) {}
}

impl<T: AsyncRead + Unpin> AsyncRead for Stream<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Stream<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// One connection, then pending forever: a stream that ends triggers tonic's
/// graceful shutdown.
pub fn one_connection<T>(io: T) -> impl tokio_stream::Stream<Item = io::Result<Stream<T>>>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use tokio_stream::StreamExt;
    tokio_stream::once(Ok(Stream(io))).chain(tokio_stream::pending())
}

/// Turns an already-connected stream into a channel, for the one call made
/// before a certificate exists.
pub async fn channel_over<T>(io: T) -> Result<tonic::transport::Channel, tonic::transport::Error>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let held = std::sync::Arc::new(tokio::sync::Mutex::new(Some(hyper_util::rt::TokioIo::new(
        io,
    ))));
    tonic::transport::Endpoint::try_from("https://relay.invalid")?
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(10))
        .connect_with_connector(tower::service_fn(move |_| {
            let held = held.clone();
            async move {
                held.lock()
                    .await
                    .take()
                    .ok_or_else(|| io::Error::other("the socket is single use"))
            }
        }))
        .await
}

/// Grows to a ceiling, with jitter, so a whole fleet reconnects spread out.
pub fn backoff(attempt: u32) -> Duration {
    const CEILING_SECS: u64 = 30;
    let seconds = (1u64 << attempt.min(5)).min(CEILING_SECS);
    let jitter = Duration::from_millis(u64::from(rand_ms()));
    Duration::from_secs(seconds) + jitter
}

/// Enough spread to break up a simultaneous reconnect, from the clock rather
/// than a dependency.
fn rand_ms() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos % 1000
}

/// Ends the process so the orchestrator restarts it with a fresh tunnel.
pub fn lost(why: &str) -> ! {
    tracing::error!(%why, "the tunnel is gone; exiting to be restarted");
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_then_stops() {
        let secs = |a| backoff(a).as_secs();
        assert_eq!(secs(0), 1);
        assert_eq!(secs(1), 2);
        assert_eq!(secs(4), 16);
        assert_eq!(secs(5), 30, "capped");
        assert_eq!(secs(50), 30, "still capped");
    }

    #[test]
    fn backoff_carries_jitter() {
        assert!(backoff(0).subsec_millis() < 1000);
    }
}
