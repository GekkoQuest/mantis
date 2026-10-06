//! What starts a drain: SIGTERM (what `docker stop` and orchestrators
//! send), Ctrl-C, or, when asked for, the end of standard input (how a
//! parent process on any platform asks a child to drain: it closes the
//! child's stdin).
//!
//! A drain is one-way: once requested it stays requested.

use std::io::Read as _;

use tokio::sync::watch;

/// The drain request, observable from synchronous and async code.
#[derive(Clone)]
pub struct Drain {
    rx: watch::Receiver<Option<&'static str>>,
    tx: std::sync::Arc<watch::Sender<Option<&'static str>>>,
}

impl Drain {
    /// Listens for SIGTERM and Ctrl-C on `handle`, and for the end of
    /// standard input when `stdin_eof`.
    #[must_use]
    pub fn install(handle: &tokio::runtime::Handle, stdin_eof: bool) -> Self {
        let (tx, rx) = watch::channel(None);
        let tx = std::sync::Arc::new(tx);
        let ctrl = std::sync::Arc::clone(&tx);
        handle.spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                ctrl.send_replace(Some("Ctrl-C"));
            }
        });
        #[cfg(unix)]
        {
            let term = std::sync::Arc::clone(&tx);
            handle.spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};
                if let Ok(mut s) = signal(SignalKind::terminate())
                    && s.recv().await.is_some()
                {
                    term.send_replace(Some("SIGTERM"));
                }
            });
        }
        if stdin_eof {
            let eof = std::sync::Arc::clone(&tx);
            let _ = std::thread::Builder::new()
                .name("drain-stdin".to_owned())
                .spawn(move || {
                    let mut sink = [0u8; 64];
                    let mut stdin = std::io::stdin();
                    while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
                    eof.send_replace(Some("standard input closed"));
                });
        }
        Self { rx, tx }
    }

    /// Requests a drain from inside the process.
    pub fn request(&self, why: &'static str) {
        self.tx.send_if_modified(|v| {
            if v.is_none() {
                *v = Some(why);
                true
            } else {
                false
            }
        });
    }

    /// Why a drain was requested, if it was.
    #[must_use]
    pub fn requested(&self) -> Option<&'static str> {
        *self.rx.borrow()
    }

    /// Waits until a drain is requested; returns why.
    pub async fn wait(&self) -> &'static str {
        let mut rx = self.rx.clone();
        loop {
            if let Some(why) = *rx.borrow_and_update() {
                return why;
            }
            if rx.changed().await.is_err() {
                return "the drain signal closed";
            }
        }
    }
}
