//! A listener must stop listening even if a concurrent fork inherited it.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::TcpListenerStream;

pub(crate) struct Listener {
    inner: TcpListenerStream,
    #[cfg(target_os = "linux")]
    shutdown_on_drop: bool,
}

impl Listener {
    pub(crate) fn new(listener: TcpListener) -> Self {
        Self {
            inner: TcpListenerStream::new(listener),
            #[cfg(target_os = "linux")]
            shutdown_on_drop: true,
        }
    }

    /// The caller keeps this socket listening across head generations.
    pub(crate) fn shared(listener: TcpListener) -> Self {
        Self {
            inner: TcpListenerStream::new(listener),
            #[cfg(target_os = "linux")]
            shutdown_on_drop: false,
        }
    }

    pub(crate) fn get(&self) -> &TcpListener {
        self.inner.as_ref()
    }
}

impl Stream for Listener {
    type Item = std::io::Result<TcpStream>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;

            // CLOEXEC closes an inherited descriptor only at exec, not at
            // fork. Closing ours alone leaves that child's copy listening.
            // Linux shutdown stops the shared socket before either closes.
            // Other platforms (including Darwin) reject listener shutdown.
            // SAFETY: we own this live listener throughout the call.
            if self.shutdown_on_drop && unsafe { libc::shutdown(self.get().as_raw_fd(), libc::SHUT_RDWR) } != 0 {
                tracing::warn!(error = %std::io::Error::last_os_error(), "shutting down a listener");
            }
        }
    }
}
