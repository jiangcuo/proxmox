//! Incoming connection handling for the Rest Server.
//!
//! Hyper building block.

use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{format_err, Context as _, Error};
use futures::FutureExt;
use hyper::server::accept;
use openssl::ec::{EcGroup, EcKey};
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
use openssl::x509::X509;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_openssl::SslStream;
use tokio_stream::wrappers::ReceiverStream;

#[cfg(feature = "rate-limited-stream")]
use proxmox_http::{RateLimitedStream, ShareableRateLimit};

#[cfg(feature = "rate-limited-stream")]
pub type SharedRateLimit = Arc<dyn ShareableRateLimit>;

enum Tls {
    KeyCert(PKey<Private>, X509),
    FilesPem(PathBuf, PathBuf),
}

/// A builder for an `SslAcceptor` which can be configured either with certificates (or path to PEM
/// files), or otherwise builds a self-signed certificate on the fly (mostly useful during
/// development).
#[derive(Default)]
pub struct TlsAcceptorBuilder {
    tls: Option<Tls>,
    cipher_suites: Option<String>,
    cipher_list: Option<String>,
}

impl TlsAcceptorBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn certificate(mut self, key: PKey<Private>, cert: X509) -> Self {
        self.tls = Some(Tls::KeyCert(key, cert));
        self
    }

    pub fn certificate_paths_pem(
        mut self,
        key: impl Into<PathBuf>,
        cert: impl Into<PathBuf>,
    ) -> Self {
        self.tls = Some(Tls::FilesPem(key.into(), cert.into()));
        self
    }

    pub fn cipher_suites(mut self, suites: String) -> Self {
        self.cipher_suites = Some(suites);
        self
    }

    pub fn cipher_list(mut self, list: String) -> Self {
        self.cipher_list = Some(list);
        self
    }

    pub fn build(self) -> Result<SslAcceptor, Error> {
        let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();

        match self.tls {
            Some(Tls::KeyCert(key, cert)) => {
                acceptor
                    .set_private_key(&key)
                    .context("failed to set tls acceptor private key")?;
                acceptor
                    .set_certificate(&cert)
                    .context("failed to set tls acceptor certificate")?;
            }
            Some(Tls::FilesPem(key, cert)) => {
                acceptor
                    .set_private_key_file(key, SslFiletype::PEM)
                    .context("failed to set tls acceptor private key file")?;
                acceptor
                    .set_certificate_chain_file(cert)
                    .context("failed to set tls acceptor certificate chain file")?;
            }
            None => {
                let key = EcKey::generate(
                    EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)
                        .context("failed to get NIST-P256 curve from openssl")?
                        .as_ref(),
                )
                .and_then(PKey::from_ec_key)
                .context("generating temporary ec key")?;
                //let key = openssl::rsa::Rsa::generate(4096)
                //    .and_then(PKey::from_rsa)
                //    .context("generating temporary rsa key")?;

                let mut cert =
                    X509::builder().context("generating building self signed certificate")?;
                cert.set_version(2)?;
                cert.set_pubkey(&key)?;
                cert.sign(&key, openssl::hash::MessageDigest::sha256())?;
                cert.set_not_before(openssl::asn1::Asn1Time::days_from_now(0)?.as_ref())?;
                cert.set_not_after(openssl::asn1::Asn1Time::days_from_now(365)?.as_ref())?;

                let mut name = openssl::x509::X509Name::builder()?;
                name.append_entry_by_text("C", "CA")?;
                name.append_entry_by_text("O", "Self")?;
                name.append_entry_by_text("CN", "localhost")?;
                cert.set_issuer_name(name.build().as_ref())?;

                let cert = cert.build();

                acceptor
                    .set_private_key(&key)
                    .context("failed to set tls acceptor private key")?;
                acceptor
                    .set_certificate(&cert)
                    .context("failed to set tls acceptor certificate")?;
            }
        }
        acceptor.set_options(openssl::ssl::SslOptions::NO_RENEGOTIATION);
        acceptor.check_private_key().unwrap();

        Ok(acceptor.build())
    }
}

#[cfg(not(feature = "rate-limited-stream"))]
type InsecureClientStream = TcpStream;
#[cfg(feature = "rate-limited-stream")]
type InsecureClientStream = RateLimitedStream<TcpStream>;

type InsecureClientStreamResult = Pin<Box<InsecureClientStream>>;

type ClientStreamResult = Pin<Box<SslStream<InsecureClientStream>>>;

#[cfg(feature = "rate-limited-stream")]
type LookupRateLimiter = dyn Fn(std::net::SocketAddr) -> (Option<SharedRateLimit>, Option<SharedRateLimit>)
    + Send
    + Sync
    + 'static;

pub struct AcceptBuilder {
    debug: bool,
    tcp_keepalive_time: u32,
    max_pending_accepts: usize,

    #[cfg(feature = "rate-limited-stream")]
    lookup_rate_limiter: Option<Arc<LookupRateLimiter>>,
}

impl Default for AcceptBuilder {
    fn default() -> Self {
        Self {
            debug: false,
            tcp_keepalive_time: 120,
            max_pending_accepts: 1024,

            #[cfg(feature = "rate-limited-stream")]
            lookup_rate_limiter: None,
        }
    }
}

impl AcceptBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    pub fn tcp_keepalive_time(mut self, time: u32) -> Self {
        self.tcp_keepalive_time = time;
        self
    }

    pub fn max_pending_accepts(mut self, count: usize) -> Self {
        self.max_pending_accepts = count;
        self
    }

    #[cfg(feature = "rate-limited-stream")]
    pub fn rate_limiter_lookup(mut self, lookup_rate_limiter: Arc<LookupRateLimiter>) -> Self {
        self.lookup_rate_limiter = Some(lookup_rate_limiter);
        self
    }
}

impl AcceptBuilder {
    pub fn accept_tls(
        self,
        listener: TcpListener,
        acceptor: Arc<Mutex<SslAcceptor>>,
    ) -> impl accept::Accept<Conn = ClientStreamResult, Error = Error> {
        let (secure_sender, secure_receiver) = mpsc::channel(self.max_pending_accepts);

        tokio::spawn(self.accept_connections(listener, acceptor, secure_sender.into()));

        accept::from_stream(ReceiverStream::new(secure_receiver))
    }

    pub fn accept_tls_optional(
        self,
        listener: TcpListener,
        acceptor: Arc<Mutex<SslAcceptor>>,
    ) -> (
        impl accept::Accept<Conn = ClientStreamResult, Error = Error>,
        impl accept::Accept<Conn = InsecureClientStreamResult, Error = Error>,
    ) {
        let (secure_sender, secure_receiver) = mpsc::channel(self.max_pending_accepts);
        let (insecure_sender, insecure_receiver) = mpsc::channel(self.max_pending_accepts);

        tokio::spawn(self.accept_connections(
            listener,
            acceptor,
            (secure_sender, insecure_sender).into(),
        ));

        (
            accept::from_stream(ReceiverStream::new(secure_receiver)),
            accept::from_stream(ReceiverStream::new(insecure_receiver)),
        )
    }
}

type ClientSender = mpsc::Sender<Result<ClientStreamResult, Error>>;
type InsecureClientSender = mpsc::Sender<Result<InsecureClientStreamResult, Error>>;

enum Sender {
    Secure(ClientSender),
    SecureAndInsecure(ClientSender, InsecureClientSender),
}

impl From<ClientSender> for Sender {
    fn from(sender: ClientSender) -> Self {
        Sender::Secure(sender)
    }
}

impl From<(ClientSender, InsecureClientSender)> for Sender {
    fn from(senders: (ClientSender, InsecureClientSender)) -> Self {
        Sender::SecureAndInsecure(senders.0, senders.1)
    }
}

impl AcceptBuilder {
    async fn accept_connections(
        self,
        listener: TcpListener,
        acceptor: Arc<Mutex<SslAcceptor>>,
        sender: Sender,
    ) {
        let accept_counter = Arc::new(());
        let mut shutdown_future = crate::shutdown_future().fuse();

        loop {
            let socket = futures::select! {
                res = self.try_setup_socket(&listener).fuse() => match res {
                    Ok(socket) => socket,
                    Err(err) => {
                        log::error!("couldn't set up TCP socket: {err}");
                        continue;
                    }
                },
                _ = shutdown_future => break,
            };

            let acceptor = Arc::clone(&acceptor);
            let accept_counter = Arc::clone(&accept_counter);

            if Arc::strong_count(&accept_counter) > self.max_pending_accepts {
                log::error!("connection rejected - too many open connections");
                continue;
            }

            match sender {
                Sender::Secure(ref secure_sender) => {
                    let accept_future = Self::do_accept_tls(
                        socket,
                        acceptor,
                        accept_counter,
                        self.debug,
                        secure_sender.clone(),
                    );

                    tokio::spawn(accept_future);
                }
                Sender::SecureAndInsecure(ref secure_sender, ref insecure_sender) => {
                    let accept_future = Self::do_accept_tls_optional(
                        socket,
                        acceptor,
                        accept_counter,
                        self.debug,
                        secure_sender.clone(),
                        insecure_sender.clone(),
                    );

                    tokio::spawn(accept_future);
                }
            };
        }
    }

    async fn try_setup_socket(
        &self,
        listener: &TcpListener,
    ) -> Result<InsecureClientStream, Error> {
        let (socket, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                return Err(format_err!(error)).context("error while accepting tcp stream")
            }
        };

        socket
            .set_nodelay(true)
            .context("error while setting TCP_NODELAY on socket")?;

        proxmox_sys::linux::socket::set_tcp_keepalive(socket.as_raw_fd(), self.tcp_keepalive_time)
            .context("error while setting SO_KEEPALIVE on socket")?;

        #[cfg(feature = "rate-limited-stream")]
        let socket = match self.lookup_rate_limiter.clone() {
            Some(lookup) => RateLimitedStream::with_limiter_update_cb(socket, move || lookup(peer)),
            None => RateLimitedStream::with_limiter(socket, None, None),
        };

        #[cfg(not(feature = "rate-limited-stream"))]
        let _peer = peer;

        Ok(socket)
    }

    async fn do_accept_tls(
        socket: InsecureClientStream,
        acceptor: Arc<Mutex<SslAcceptor>>,
        accept_counter: Arc<()>,
        debug: bool,
        secure_sender: ClientSender,
    ) {
        let ssl = {
            // limit acceptor_guard scope
            // Acceptor can be reloaded using the command socket "reload-certificate" command
            let acceptor_guard = acceptor.lock().unwrap();

            match openssl::ssl::Ssl::new(acceptor_guard.context()) {
                Ok(ssl) => ssl,
                Err(err) => {
                    log::error!("failed to create Ssl object from Acceptor context - {err}");
                    return;
                }
            }
        };

        let secure_stream = match tokio_openssl::SslStream::new(ssl, socket) {
            Ok(stream) => stream,
            Err(err) => {
                log::error!("failed to create SslStream using ssl and connection socket - {err}");
                return;
            }
        };

        let mut secure_stream = Box::pin(secure_stream);

        let accept_future =
            tokio::time::timeout(Duration::new(10, 0), secure_stream.as_mut().accept());

        let result = accept_future.await;

        match result {
            Ok(Ok(())) => {
                if secure_sender.send(Ok(secure_stream)).await.is_err() && debug {
                    log::error!("detected closed connection channel");
                }
            }
            Ok(Err(err)) => {
                if debug {
                    log::error!("https handshake failed - {err}");
                }
            }
            Err(_) => {
                if debug {
                    log::error!("https handshake timeout");
                }
            }
        }

        drop(accept_counter); // decrease reference count
    }

    async fn do_accept_tls_optional(
        socket: InsecureClientStream,
        acceptor: Arc<Mutex<SslAcceptor>>,
        accept_counter: Arc<()>,
        debug: bool,
        secure_sender: ClientSender,
        insecure_sender: InsecureClientSender,
    ) {
        let client_initiates_handshake = {
            #[cfg(feature = "rate-limited-stream")]
            let socket = socket.inner();

            #[cfg(not(feature = "rate-limited-stream"))]
            let socket = &socket;

            match Self::wait_for_client_tls_handshake(socket).await {
                Ok(initiates_handshake) => initiates_handshake,
                Err(err) => {
                    log::error!("error checking for TLS handshake: {err}");
                    return;
                }
            }
        };

        if !client_initiates_handshake {
            let insecure_stream = Box::pin(socket);

            if insecure_sender.send(Ok(insecure_stream)).await.is_err() && debug {
                log::error!("detected closed connection channel")
            }

            return;
        }

        Self::do_accept_tls(socket, acceptor, accept_counter, debug, secure_sender).await
    }

    async fn wait_for_client_tls_handshake(incoming_stream: &TcpStream) -> Result<bool, Error> {
        const MS_TIMEOUT: u64 = 1000;
        const BYTES_BUF_SIZE: usize = 128;

        let mut buf = [0; BYTES_BUF_SIZE];
        let mut last_peek_size = 0;

        let future = async {
            loop {
                let peek_size = incoming_stream
                    .peek(&mut buf)
                    .await
                    .context("couldn't peek into incoming tcp stream")?;

                if contains_tls_handshake_fragment(&buf) {
                    return Ok(true);
                }

                // No more new data came in
                if peek_size == last_peek_size {
                    return Ok(false);
                }

                last_peek_size = peek_size;

                // explicitly yield to event loop; this future otherwise blocks ad infinitum
                tokio::task::yield_now().await;
            }
        };

        tokio::time::timeout(Duration::from_millis(MS_TIMEOUT), future)
            .await
            .unwrap_or(Ok(false))
    }
}

/// Checks whether an [SSL 3.0 / TLS plaintext fragment][0] being part of a
/// SSL / TLS handshake is contained in the given buffer.
///
/// Such a fragment might look as follows:
/// ```ignore
/// [0x16, 0x3, 0x1, 0x02, 0x00, ...]
/// //  |    |    |     |_____|
/// //  |    |    |            \__ content length interpreted as u16
/// //  |    |    |                must not exceed 0x4000 (2^14) bytes
/// //  |    |    |
/// //  |    |     \__ any minor version
/// //  |    |
/// //  |     \__ major version 3
/// //  |
/// //   \__ content type is handshake(22)
/// ```
///
/// If a slice like this is detected at the beginning of the given buffer,
/// a TLS handshake is most definitely being made.
///
/// [0]: https://datatracker.ietf.org/doc/html/rfc6101#section-5.2
#[inline]
fn contains_tls_handshake_fragment(buf: &[u8]) -> bool {
    const SLICE_LENGTH: usize = 5;
    const CONTENT_SIZE: u16 = 1 << 14; // max length of a TLS plaintext fragment

    if buf.len() < SLICE_LENGTH {
        return false;
    }

    buf[0] == 0x16 && buf[1] == 0x3 && (((buf[3] as u16) << 8) + buf[4] as u16) <= CONTENT_SIZE
}
