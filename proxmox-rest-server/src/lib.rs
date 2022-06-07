//! # Proxmox REST server
//!
//! This module provides convenient building blocks to implement a
//! REST server.
//!
//! ## Features
//!
//! * highly threaded code, uses Rust async
//! * static API definitions using schemas
//! * restartable systemd daemons using `systemd_notify`
//! * support for long running worker tasks (threads or async tokio tasks)
//! * supports separate access and authentication log files
//! * extra control socket to trigger management operations
//!   - logfile rotation
//!   - worker task management
//! * generic interface to authenticate user

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, format_err, Error};
use http::request::Parts;
use http::HeaderMap;
use hyper::{Body, Method, Response};
use nix::unistd::Pid;

use proxmox_router::UserInformation;
use proxmox_sys::fd::Fd;
use proxmox_sys::fs::CreateOptions;
use proxmox_sys::linux::procfs::PidStat;

mod compression;
pub use compression::*;

pub mod daemon;

pub mod formatter;

mod environment;
pub use environment::*;

mod state;
pub use state::*;

mod command_socket;
pub use command_socket::*;

mod file_logger;
pub use file_logger::{FileLogOptions, FileLogger};

mod api_config;
pub use api_config::ApiConfig;

mod rest;
pub use rest::RestServer;

mod worker_task;
pub use worker_task::*;

mod h2service;
pub use h2service::*;

/// Authentication Error
pub enum AuthError {
    Generic(Error),
    NoData,
}

impl From<Error> for AuthError {
    fn from(err: Error) -> Self {
        AuthError::Generic(err)
    }
}

/// User Authentication and index/root page generation methods
pub trait ServerAdapter: Send + Sync {
    /// Returns the index/root page
    fn get_index(
        &self,
        rest_env: RestEnvironment,
        parts: Parts,
    ) -> Pin<Box<dyn Future<Output = Response<Body>> + Send>>;

    /// Extract user credentials from headers and check them.
    ///
    /// If credenthials are valid, returns the username and a
    /// [UserInformation] object to query additional user data.
    fn check_auth<'a>(
        &'a self,
        headers: &'a HeaderMap,
        method: &'a Method,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<(String, Box<dyn UserInformation + Sync + Send>), AuthError>>
                + Send
                + 'a,
        >,
    >;
}

lazy_static::lazy_static! {
    static ref PID: i32 = unsafe { libc::getpid() };
    static ref PSTART: u64 = PidStat::read_from_pid(Pid::from_raw(*PID)).unwrap().starttime;
}

/// Returns the current process ID (see [libc::getpid])
///
/// The value is cached at startup (so it is invalid after a fork)
pub(crate) fn pid() -> i32 {
    *PID
}

/// Returns the starttime of the process (see [PidStat])
///
/// The value is cached at startup (so it is invalid after a fork)
pub(crate) fn pstart() -> u64 {
    *PSTART
}

/// Helper to write the PID into a file
pub fn write_pid(pid_fn: &str) -> Result<(), Error> {
    let pid_str = format!("{}\n", *PID);
    proxmox_sys::fs::replace_file(pid_fn, pid_str.as_bytes(), CreateOptions::new(), false)
}

/// Helper to read the PID from a file
pub fn read_pid(pid_fn: &str) -> Result<i32, Error> {
    let pid = proxmox_sys::fs::file_get_contents(pid_fn)?;
    let pid = std::str::from_utf8(&pid)?.trim();
    pid.parse()
        .map_err(|err| format_err!("could not parse pid - {}", err))
}

/// Returns the control socket path for a specific process ID.
///
/// Note: The control socket always uses @/run/proxmox-backup/ as
/// prefix for historic reason. This does not matter because the
/// generated path is unique for each ``pid`` anyways.
pub fn ctrl_sock_from_pid(pid: i32) -> String {
    // Note: The control socket always uses @/run/proxmox-backup/ as prefix
    // for historc reason.
    format!("\0{}/control-{}.sock", "/run/proxmox-backup", pid)
}

/// Returns the control socket path for this server.
pub fn our_ctrl_sock() -> String {
    ctrl_sock_from_pid(*PID)
}

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Request a server shutdown (usually called from [catch_shutdown_signal])
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    crate::server_shutdown();
}

/// Returns true if there was a shutdown request.
#[inline(always)]
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

/// Raise an error if there was a shutdown request.
pub fn fail_on_shutdown() -> Result<(), Error> {
    if shutdown_requested() {
        bail!("Server shutdown requested - aborting task");
    }
    Ok(())
}

/// safe wrapper for `nix::sys::socket::socketpair` defaulting to `O_CLOEXEC` and guarding the file
/// descriptors.
pub fn socketpair() -> Result<(Fd, Fd), Error> {
    use nix::sys::socket;
    let (pa, pb) = socket::socketpair(
        socket::AddressFamily::Unix,
        socket::SockType::Stream,
        None,
        socket::SockFlag::SOCK_CLOEXEC,
    )?;
    Ok((Fd(pa), Fd(pb)))
}

/// Extract a specific cookie from cookie header.
/// We assume cookie_name is already url encoded.
pub fn extract_cookie(cookie: &str, cookie_name: &str) -> Option<String> {
    for pair in cookie.split(';') {
        let (name, value) = match pair.find('=') {
            Some(i) => (pair[..i].trim(), pair[(i + 1)..].trim()),
            None => return None, // Cookie format error
        };

        if name == cookie_name {
            use percent_encoding::percent_decode;
            if let Ok(value) = percent_decode(value.as_bytes()).decode_utf8() {
                return Some(value.into());
            } else {
                return None; // Cookie format error
            }
        }
    }

    None
}

/// Extract a specific cookie from a HeaderMap's "COOKIE" entry.
/// We assume cookie_name is already url encoded.
pub fn cookie_from_header(headers: &http::HeaderMap, cookie_name: &str) -> Option<String> {
    if let Some(Ok(cookie)) = headers.get("COOKIE").map(|v| v.to_str()) {
        extract_cookie(cookie, cookie_name)
    } else {
        None
    }
}

/// normalize uri path
///
/// Do not allow ".", "..", or hidden files ".XXXX"
/// Also remove empty path components
pub fn normalize_uri_path(path: &str) -> Result<(String, Vec<&str>), Error> {
    let items = path.split('/');

    let mut path = String::new();
    let mut components = vec![];

    for name in items {
        if name.is_empty() {
            continue;
        }
        if name.starts_with('.') {
            bail!("Path contains illegal components.");
        }
        path.push('/');
        path.push_str(name);
        components.push(name);
    }

    Ok((path, components))
}
