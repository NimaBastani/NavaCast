//! NavaCast: ingest, AutoDJ, fanout, and the control API.
//!
//! The audio encoder is an [`encoder::AudioFactory`]. Each mount picks ffmpeg,
//! the in-process native MP3 encoder, or the in-process Opus encoder. Tests
//! use an in-process factory so protocol, takeover, and fanout still run
//! without ffmpeg.

pub mod api;
pub mod auth;
pub mod config;
pub mod daemon;
pub mod encoder;
pub mod engine;
pub mod error;
pub mod hls;
pub mod httputil;
pub mod icy;
pub mod library;
pub mod limit;
pub mod media;
pub mod pcm;
pub mod record;
pub mod relay;
pub mod schedule;
pub mod store;
pub mod stream;

pub use error::Error;

/// Join `handle`, but return after `timeout` if it is still running.
/// A stuck encoder or listener must not keep the process alive.
pub fn join_timeout(handle: std::thread::JoinHandle<()>, timeout: std::time::Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });
    rx.recv_timeout(timeout).is_ok()
}

pub fn ignore_sigpipe() {
    // Closed listener sockets must return EPIPE instead of killing the process.
    // Windows has no SIGPIPE; a write on a closed socket already returns an error.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}
