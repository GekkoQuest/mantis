//! mantis-net: transports and the handshake (plan 9, decision 0002).
//!
//! - [`quic`]: the native transport: QUIC with one reliable stream and RFC
//!   9221 datagrams under the Mantis sequence header.
//! - [`tcp`]: the transport for legacy-shaped adapters.
//! - [`handshake`]: version, capability, content-hash, module, and token
//!   negotiation for a `Hello`.
//!
//! Both transports implement `mantis_adapter_contract::Transport`: a
//! synchronous polling facade whose network I/O runs on a [`NetRuntime`], so
//! no async runtime leaks into a cell or simulation thread. On the caller
//! side, `poll` and `send` are allocation-free in steady state (pooled frame
//! buffers).

#![forbid(unsafe_code)]

pub mod handshake;
pub mod quic;
mod shared;
pub mod tcp;

use core::fmt;

/// The network I/O runtime shared by transports.
pub struct NetRuntime {
    runtime: tokio::runtime::Runtime,
}

impl fmt::Debug for NetRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetRuntime").finish_non_exhaustive()
    }
}

impl NetRuntime {
    /// A runtime with `threads` network worker threads.
    ///
    /// # Errors
    /// [`NetError::Io`] if the threads cannot start.
    pub fn new(threads: usize) -> Result<Self, NetError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads.max(1))
            .thread_name("mantis-net")
            .enable_all()
            .build()
            .map_err(|e| NetError::Io(e.kind()))?;
        Ok(Self { runtime })
    }

    pub(crate) fn handle(&self) -> &tokio::runtime::Handle {
        self.runtime.handle()
    }
}

/// A transport could not be created.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NetError {
    /// A socket operation failed.
    Io(std::io::ErrorKind),
    /// TLS configuration failed.
    Tls(String),
    /// Connecting failed.
    Connect(String),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(k) => write!(f, "network I/O failed: {k}"),
            Self::Tls(e) => write!(f, "TLS setup failed: {e}"),
            Self::Connect(e) => write!(f, "connect failed: {e}"),
        }
    }
}

impl std::error::Error for NetError {}
