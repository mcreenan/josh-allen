#![forbid(unsafe_code)]

//! Implementation of the restricted synchronous HTTPS broker.
//!
//! This crate owns the network trust boundary. It has no dependency on the
//! compiler, bytecode, VM, runtime, CLI, or filesystem crates.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use flate2::bufread::GzDecoder;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use url::{Host, Url};

const USER_AGENT: &str = "allen-http-get/0.1";
const ACCEPT_ENCODING: &str = "gzip";
const MAX_CHUNK_LINE_BYTES: usize = 1_024;

/// Finite ceilings applied to one HTTP execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HttpLimits {
    pub max_requests: u32,
    pub max_redirects: u32,
    pub max_url_bytes: usize,
    pub max_dns_candidates: usize,
    pub max_response_headers: usize,
    pub max_header_bytes: usize,
    pub max_compressed_bytes: u64,
    pub max_decoded_bytes: u64,
    pub max_decompression_ratio: u32,
    pub dns_timeout: Duration,
    pub connect_timeout: Duration,
    pub first_byte_timeout: Duration,
    pub idle_timeout: Duration,
    pub total_timeout: Duration,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_requests: 100,
            max_redirects: 10,
            max_url_bytes: 8_192,
            max_dns_candidates: 32,
            max_response_headers: 256,
            max_header_bytes: 64 * 1_024,
            max_compressed_bytes: 16 * 1_024 * 1_024,
            max_decoded_bytes: 16 * 1_024 * 1_024,
            max_decompression_ratio: 100,
            dns_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(10),
            first_byte_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(30),
        }
    }
}

impl HttpLimits {
    /// Reject a limit set that could disable a security bound.
    ///
    /// # Errors
    ///
    /// Returns `InvalidLimits` when any ceiling or deadline is zero.
    pub fn validate(self) -> Result<Self, HttpError> {
        if self.max_requests == 0
            || self.max_url_bytes == 0
            || self.max_dns_candidates == 0
            || self.max_response_headers == 0
            || self.max_header_bytes == 0
            || self.max_compressed_bytes == 0
            || self.max_decoded_bytes == 0
            || self.max_decompression_ratio == 0
            || self.dns_timeout.is_zero()
            || self.connect_timeout.is_zero()
            || self.first_byte_timeout.is_zero()
            || self.idle_timeout.is_zero()
            || self.total_timeout.is_zero()
        {
            return Err(HttpError::new(
                HttpErrorCode::InvalidLimits,
                "HTTP limits must be finite and greater than zero",
            ));
        }
        Ok(self)
    }
}

/// Charged HTTP use for one execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HttpUsage {
    pub requests: u32,
    pub redirects: u32,
    pub response_headers: u64,
    pub header_bytes: u64,
    pub compressed_bytes: u64,
    pub decoded_bytes: u64,
}

/// Stable, language-safe HTTP failure codes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpErrorCode {
    InvalidLimits,
    InvalidUrl,
    OriginDenied,
    DestinationDenied,
    Dns,
    DnsTimeout,
    PeerMismatch,
    ConnectTimeout,
    FirstByteTimeout,
    IdleTimeout,
    TotalTimeout,
    RequestLimit,
    RedirectLimit,
    RedirectInvalid,
    HeaderLimit,
    Protocol,
    CompressedLimit,
    DecodedLimit,
    DecompressionRatio,
    UnsupportedEncoding,
    Tls,
    Io,
}

impl HttpErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidLimits => "net.invalid_limits",
            Self::InvalidUrl => "net.invalid_url",
            Self::OriginDenied => "net.origin_denied",
            Self::DestinationDenied => "net.destination_denied",
            Self::Dns => "net.dns",
            Self::DnsTimeout => "net.dns_timeout",
            Self::PeerMismatch => "net.peer_mismatch",
            Self::ConnectTimeout => "net.connect_timeout",
            Self::FirstByteTimeout => "net.first_byte_timeout",
            Self::IdleTimeout => "net.idle_timeout",
            Self::TotalTimeout => "net.total_timeout",
            Self::RequestLimit => "resource.http_requests",
            Self::RedirectLimit => "resource.http_redirects",
            Self::RedirectInvalid => "net.redirect_invalid",
            Self::HeaderLimit => "resource.http_header_bytes",
            Self::Protocol => "net.protocol",
            Self::CompressedLimit => "resource.http_compressed_bytes",
            Self::DecodedLimit => "resource.http_decoded_bytes",
            Self::DecompressionRatio => "resource.http_decompression_ratio",
            Self::UnsupportedEncoding => "net.unsupported_encoding",
            Self::Tls => "net.tls",
            Self::Io => "net.io",
        }
    }
}

impl fmt::Display for HttpErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One bounded failure without URL, address, credential, or provider detail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpError {
    pub code: HttpErrorCode,
    pub message: &'static str,
}

impl HttpError {
    const fn new(code: HttpErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for HttpError {}

/// A normalized HTTPS origin used by manifest and host policy.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CanonicalOrigin(String);

impl CanonicalOrigin {
    /// Parse one origin with no path, query, fragment, or credentials.
    ///
    /// # Errors
    ///
    /// Returns `InvalidUrl` unless `value` is its canonical HTTPS origin form.
    pub fn parse(value: &str) -> Result<Self, HttpError> {
        let url = parse_https_url(value, usize::MAX)?;
        if url.path() != "/" || url.query().is_some() {
            return Err(invalid_url());
        }
        let origin = Self::from_url(&url)?;
        if origin.as_str() != value {
            return Err(invalid_url());
        }
        Ok(origin)
    }

    fn from_url(url: &Url) -> Result<Self, HttpError> {
        let host = match url.host().ok_or_else(invalid_url)? {
            Host::Domain(host) => host.to_owned(),
            Host::Ipv4(host) => host.to_string(),
            Host::Ipv6(host) => format!("[{host}]"),
        };
        let port = url.port_or_known_default().ok_or_else(invalid_url)?;
        let text = if port == 443 {
            format!("https://{host}")
        } else {
            format!("https://{host}:{port}")
        };
        Ok(Self(text))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The source-visible HTTP response. Non-2xx status is not an error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub final_url: String,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: Vec<u8>,
}

/// A monotonic execution clock. Tests can inject a deterministic clock.
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
}

/// The production monotonic clock.
#[derive(Debug)]
pub struct SystemClock(Instant);

impl Default for SystemClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// Resolve one already-normalized host under a finite deadline.
pub trait Resolver: Send + Sync {
    /// # Errors
    ///
    /// Returns a safe DNS or timeout failure.
    fn resolve(
        &self,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<Vec<SocketAddr>, HttpError>;
}

/// System name resolution isolated behind a finite caller wait.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolver;

impl Resolver for SystemResolver {
    fn resolve(
        &self,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<Vec<SocketAddr>, HttpError> {
        let host = host.to_owned();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("allen-http-dns".to_owned())
            .spawn(move || {
                let result = (host.as_str(), port)
                    .to_socket_addrs()
                    .map(Iterator::collect::<Vec<_>>);
                let _ = sender.send(result);
            })
            .map_err(|_| HttpError::new(HttpErrorCode::Dns, "DNS resolution failed"))?;
        match receiver.recv_timeout(timeout) {
            Ok(Ok(addresses)) => Ok(addresses),
            Ok(Err(_)) => Err(HttpError::new(HttpErrorCode::Dns, "DNS resolution failed")),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(HttpError::new(
                HttpErrorCode::DnsTimeout,
                "DNS resolution exceeded its deadline",
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(HttpError::new(HttpErrorCode::Dns, "DNS resolution failed"))
            }
        }
    }
}

/// Exact per-hop request passed to the pinned transport.
#[derive(Clone, Debug)]
pub struct TransportRequest {
    pub url: Url,
    pub selected_address: SocketAddr,
    pub max_response_headers: usize,
    pub max_header_bytes: usize,
    pub max_compressed_bytes: u64,
    pub connect_timeout: Duration,
    pub first_byte_timeout: Duration,
    pub idle_timeout: Duration,
    pub total_timeout: Duration,
}

impl TransportRequest {
    #[must_use]
    pub const fn user_agent(&self) -> &'static str {
        USER_AGENT
    }

    #[must_use]
    pub const fn accept_encoding(&self) -> &'static str {
        ACCEPT_ENCODING
    }
}

/// Raw response returned by one pinned HTTPS connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportResponse {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub header_bytes: usize,
    pub compressed_body: Vec<u8>,
    pub peer_address: SocketAddr,
}

/// A transport cannot redirect, resolve another name, use a proxy, or retain state.
pub trait Transport: Send + Sync {
    /// # Errors
    ///
    /// Returns a safe TLS, protocol, I/O, or deadline failure.
    fn get(&self, request: &TransportRequest) -> Result<TransportResponse, HttpError>;
}

/// Stateless HTTP/1.1 over rustls with Mozilla roots and no client certificate.
#[derive(Clone)]
pub struct RustlsTransport {
    tls: Arc<ClientConfig>,
}

impl fmt::Debug for RustlsTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RustlsTransport")
            .finish_non_exhaustive()
    }
}

impl Default for RustlsTransport {
    fn default() -> Self {
        let roots = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .cloned()
            .collect::<RootCertStore>();
        let tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Self { tls: Arc::new(tls) }
    }
}

impl Transport for RustlsTransport {
    fn get(&self, request: &TransportRequest) -> Result<TransportResponse, HttpError> {
        let started = Instant::now();
        let connect = request.connect_timeout.min(request.total_timeout);
        let socket = TcpStream::connect_timeout(&request.selected_address, connect)
            .map_err(|error| map_connect_error(&error))?;
        socket
            .set_read_timeout(Some(connect))
            .map_err(|_| io_error())?;
        socket
            .set_write_timeout(Some(connect))
            .map_err(|_| io_error())?;
        let peer = socket.peer_addr().map_err(|_| io_error())?;
        if peer != request.selected_address {
            return Err(peer_mismatch());
        }

        let host = request.url.host_str().ok_or_else(invalid_url)?;
        let server_name = ServerName::try_from(host.to_owned()).map_err(|_| invalid_url())?;
        let connection = ClientConnection::new(Arc::clone(&self.tls), server_name)
            .map_err(|_| HttpError::new(HttpErrorCode::Tls, "TLS setup failed"))?;
        let mut stream = StreamOwned::new(connection, socket);
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|error| map_tls_io(&error, HttpErrorCode::ConnectTimeout))?;

        let target = request_target(&request.url);
        let authority = request_authority(&request.url)?;
        write!(
            stream,
            "GET {target} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: {USER_AGENT}\r\nAccept-Encoding: {ACCEPT_ENCODING}\r\nConnection: close\r\n\r\n"
        )
        .map_err(|_| io_error())?;
        stream.flush().map_err(|_| io_error())?;

        let (status, headers, prefix, header_bytes) =
            read_response_head(&mut stream, request, started)?;
        let compressed_body =
            read_response_body(&mut stream, prefix, status, &headers, request, started)?;
        Ok(TransportResponse {
            status,
            headers,
            header_bytes,
            compressed_body,
            peer_address: peer,
        })
    }
}

/// Execution-scoped restricted GET broker.
pub struct HttpBroker {
    origins: BTreeSet<CanonicalOrigin>,
    denied_addresses: BTreeSet<IpAddr>,
    limits: HttpLimits,
    usage: HttpUsage,
    resolver: Box<dyn Resolver>,
    transport: Box<dyn Transport>,
    clock: Box<dyn Clock>,
}

impl fmt::Debug for HttpBroker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpBroker")
            .field("origins", &self.origins)
            .field("limits", &self.limits)
            .field("usage", &self.usage)
            .finish_non_exhaustive()
    }
}

impl HttpBroker {
    /// Construct a broker from an already-intersected execution policy.
    ///
    /// # Errors
    ///
    /// Returns a stable error for an invalid limit set or origin.
    pub fn new(
        origins: impl IntoIterator<Item = String>,
        limits: HttpLimits,
        resolver: Box<dyn Resolver>,
        transport: Box<dyn Transport>,
        clock: Box<dyn Clock>,
    ) -> Result<Self, HttpError> {
        Self::new_with_denied_addresses(
            origins,
            BTreeSet::new(),
            limits,
            resolver,
            transport,
            clock,
        )
    }

    /// Construct a broker with explicit host-denied destination addresses.
    ///
    /// # Errors
    ///
    /// Returns a stable error for an invalid limit set or origin.
    pub fn new_with_denied_addresses(
        origins: impl IntoIterator<Item = String>,
        denied_addresses: BTreeSet<IpAddr>,
        limits: HttpLimits,
        resolver: Box<dyn Resolver>,
        transport: Box<dyn Transport>,
        clock: Box<dyn Clock>,
    ) -> Result<Self, HttpError> {
        let limits = limits.validate()?;
        let origins = origins
            .into_iter()
            .map(|value| CanonicalOrigin::parse(&value))
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(Self {
            origins,
            denied_addresses,
            limits,
            usage: HttpUsage::default(),
            resolver,
            transport,
            clock,
        })
    }

    /// Construct the production broker. It has no proxy, cookie, credential,
    /// client-certificate, cache, or connection-pool input.
    ///
    /// # Errors
    ///
    /// Returns the errors from [`Self::new`].
    pub fn production(
        origins: impl IntoIterator<Item = String>,
        limits: HttpLimits,
    ) -> Result<Self, HttpError> {
        Self::production_with_denied_addresses(origins, BTreeSet::new(), limits)
    }

    /// Construct the production broker with host-denied destinations.
    ///
    /// # Errors
    ///
    /// Returns the errors from [`Self::new_with_denied_addresses`].
    pub fn production_with_denied_addresses(
        origins: impl IntoIterator<Item = String>,
        denied_addresses: BTreeSet<IpAddr>,
        limits: HttpLimits,
    ) -> Result<Self, HttpError> {
        Self::new_with_denied_addresses(
            origins,
            denied_addresses,
            limits,
            Box::new(SystemResolver),
            Box::new(RustlsTransport::default()),
            Box::new(SystemClock::default()),
        )
    }

    #[must_use]
    pub const fn limits(&self) -> HttpLimits {
        self.limits
    }

    #[must_use]
    pub const fn usage(&self) -> HttpUsage {
        self.usage
    }

    /// Execute one HTTPS GET. The caller supplies only the URL.
    ///
    /// # Errors
    ///
    /// Returns a stable safe error for policy, destination, redirect,
    /// transport, deadline, or resource failure.
    pub fn get(&mut self, value: &str) -> Result<HttpResponse, HttpError> {
        let started = self.clock.now();
        let mut url = parse_https_url(value, self.limits.max_url_bytes)?;
        loop {
            self.check_total(started)?;
            self.require_origin(&url)?;
            self.charge_request()?;
            let selected = self.resolve_selected(&url, started)?;
            let remaining = self.remaining_total(started)?;
            let request = TransportRequest {
                url: url.clone(),
                selected_address: selected,
                max_response_headers: self.remaining_response_headers()?,
                max_header_bytes: self.remaining_header_bytes()?,
                max_compressed_bytes: self.remaining_compressed_bytes()?,
                connect_timeout: self.limits.connect_timeout.min(remaining),
                first_byte_timeout: self.limits.first_byte_timeout.min(remaining),
                idle_timeout: self.limits.idle_timeout.min(remaining),
                total_timeout: remaining,
            };
            let raw = self.transport.get(&request)?;
            self.check_total(started)?;
            if raw.peer_address != selected {
                return Err(peer_mismatch());
            }
            self.charge_headers(raw.headers.len(), raw.header_bytes)?;
            let headers = Self::normalize_headers(&raw.headers)?;
            self.charge_compressed(raw.compressed_body.len())?;

            if is_redirect(raw.status) {
                self.charge_redirect()?;
                let locations = headers.get("location").ok_or_else(redirect_invalid)?;
                if locations.len() != 1 {
                    return Err(redirect_invalid());
                }
                validate_redirect_reference(&locations[0], self.limits.max_url_bytes)?;
                url = url.join(&locations[0]).map_err(|_| redirect_invalid())?;
                validate_https_url(&url, self.limits.max_url_bytes)
                    .map_err(|_| redirect_invalid())?;
                continue;
            }

            let mut decode_limits = self.limits;
            decode_limits.max_decoded_bytes = self.remaining_decoded_bytes()?;
            let body = decode_body(
                &headers,
                &raw.compressed_body,
                decode_limits,
                &mut self.usage.decoded_bytes,
            )?;
            return Ok(HttpResponse {
                status: raw.status,
                final_url: url.to_string(),
                headers,
                body,
            });
        }
    }

    fn require_origin(&self, url: &Url) -> Result<(), HttpError> {
        let origin = CanonicalOrigin::from_url(url)?;
        if self.origins.contains(&origin) {
            Ok(())
        } else {
            Err(HttpError::new(
                HttpErrorCode::OriginDenied,
                "the URL origin is not allowed",
            ))
        }
    }

    fn resolve_selected(&self, url: &Url, started: Duration) -> Result<SocketAddr, HttpError> {
        let port = url.port_or_known_default().ok_or_else(invalid_url)?;
        let mut addresses = match url.host().ok_or_else(invalid_url)? {
            Host::Ipv4(address) => vec![SocketAddr::new(IpAddr::V4(address), port)],
            Host::Ipv6(address) => vec![SocketAddr::new(IpAddr::V6(address), port)],
            Host::Domain(host) => {
                let remaining = self.remaining_total(started)?;
                let before = self.clock.now();
                let result =
                    self.resolver
                        .resolve(host, port, self.limits.dns_timeout.min(remaining))?;
                let elapsed = self.clock.now().saturating_sub(before);
                if elapsed > self.limits.dns_timeout {
                    return Err(HttpError::new(
                        HttpErrorCode::DnsTimeout,
                        "DNS resolution exceeded its deadline",
                    ));
                }
                result
            }
        };
        if addresses.is_empty() || addresses.len() > self.limits.max_dns_candidates {
            return Err(HttpError::new(
                HttpErrorCode::Dns,
                "DNS returned an invalid candidate set",
            ));
        }
        if addresses.iter().any(|address| {
            address.port() != port
                || is_denied_address(address.ip())
                || self.denied_addresses.contains(&address.ip())
        }) {
            return Err(HttpError::new(
                HttpErrorCode::DestinationDenied,
                "the destination address is denied",
            ));
        }
        addresses.sort_unstable();
        addresses.dedup();
        addresses
            .first()
            .copied()
            .ok_or_else(|| HttpError::new(HttpErrorCode::Dns, "DNS returned no destination"))
    }

    fn normalize_headers(
        raw: &[(String, Vec<u8>)],
    ) -> Result<BTreeMap<String, Vec<String>>, HttpError> {
        let mut output = BTreeMap::<String, Vec<String>>::new();
        for (name, value) in raw {
            if !valid_header_name(name) {
                return Err(protocol_error());
            }
            let value = std::str::from_utf8(value).map_err(|_| protocol_error())?;
            if value.contains(['\r', '\n', '\0']) {
                return Err(protocol_error());
            }
            output
                .entry(name.to_ascii_lowercase())
                .or_default()
                .push(value.trim().to_owned());
        }
        Ok(output)
    }

    fn charge_headers(&mut self, count: usize, bytes: usize) -> Result<(), HttpError> {
        let count = u64::try_from(count).map_err(|_| header_limit())?;
        let bytes = u64::try_from(bytes).map_err(|_| header_limit())?;
        let next_count = self
            .usage
            .response_headers
            .checked_add(count)
            .ok_or_else(header_limit)?;
        let maximum_count =
            u64::try_from(self.limits.max_response_headers).map_err(|_| header_limit())?;
        let next_bytes = self
            .usage
            .header_bytes
            .checked_add(bytes)
            .ok_or_else(header_limit)?;
        if next_count > maximum_count || next_bytes > self.limits.max_header_bytes as u64 {
            return Err(header_limit());
        }
        self.usage.response_headers = next_count;
        self.usage.header_bytes = next_bytes;
        Ok(())
    }

    fn charge_request(&mut self) -> Result<(), HttpError> {
        let next = self
            .usage
            .requests
            .checked_add(1)
            .ok_or_else(request_limit)?;
        if next > self.limits.max_requests {
            return Err(request_limit());
        }
        self.usage.requests = next;
        Ok(())
    }

    fn charge_redirect(&mut self) -> Result<(), HttpError> {
        let next = self
            .usage
            .redirects
            .checked_add(1)
            .ok_or_else(redirect_limit)?;
        if next > self.limits.max_redirects {
            return Err(redirect_limit());
        }
        self.usage.redirects = next;
        Ok(())
    }

    fn charge_compressed(&mut self, bytes: usize) -> Result<(), HttpError> {
        let bytes = u64::try_from(bytes).map_err(|_| compressed_limit())?;
        if bytes > self.limits.max_compressed_bytes {
            return Err(compressed_limit());
        }
        let next = self
            .usage
            .compressed_bytes
            .checked_add(bytes)
            .ok_or_else(compressed_limit)?;
        if next > self.limits.max_compressed_bytes {
            return Err(compressed_limit());
        }
        self.usage.compressed_bytes = next;
        Ok(())
    }

    fn remaining_response_headers(&self) -> Result<usize, HttpError> {
        let used = usize::try_from(self.usage.response_headers).map_err(|_| header_limit())?;
        self.limits
            .max_response_headers
            .checked_sub(used)
            .ok_or_else(header_limit)
    }

    fn remaining_header_bytes(&self) -> Result<usize, HttpError> {
        let used = usize::try_from(self.usage.header_bytes).map_err(|_| header_limit())?;
        self.limits
            .max_header_bytes
            .checked_sub(used)
            .ok_or_else(header_limit)
    }

    fn remaining_compressed_bytes(&self) -> Result<u64, HttpError> {
        self.limits
            .max_compressed_bytes
            .checked_sub(self.usage.compressed_bytes)
            .ok_or_else(compressed_limit)
    }

    fn remaining_decoded_bytes(&self) -> Result<u64, HttpError> {
        self.limits
            .max_decoded_bytes
            .checked_sub(self.usage.decoded_bytes)
            .ok_or_else(decoded_limit)
    }

    fn remaining_total(&self, started: Duration) -> Result<Duration, HttpError> {
        self.limits
            .total_timeout
            .checked_sub(self.clock.now().saturating_sub(started))
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(total_timeout)
    }

    fn check_total(&self, started: Duration) -> Result<(), HttpError> {
        self.remaining_total(started).map(|_| ())
    }
}

fn parse_https_url(value: &str, maximum: usize) -> Result<Url, HttpError> {
    validate_raw_url_text(value, maximum)?;
    let url = Url::parse(value).map_err(|_| invalid_url())?;
    validate_https_url(&url, maximum)?;
    Ok(url)
}

fn validate_redirect_reference(value: &str, maximum: usize) -> Result<(), HttpError> {
    validate_raw_url_text(value, maximum).map_err(|_| redirect_invalid())?;
    Ok(())
}

fn validate_raw_url_text(value: &str, maximum: usize) -> Result<(), HttpError> {
    if value.is_empty()
        || value.len() > maximum
        || value.chars().any(char::is_control)
        || value.contains('\\')
        || has_invalid_percent_encoding(value.as_bytes())
    {
        return Err(invalid_url());
    }
    validate_raw_authority(value)
}

fn validate_raw_authority(value: &str) -> Result<(), HttpError> {
    let Some(scheme_end) = value.find("://") else {
        return Ok(());
    };
    let remainder = &value[scheme_end + 3..];
    let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    if authority.is_empty() || authority.contains('%') {
        return Err(invalid_url());
    }
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, value)| value);
    let port = if host_port.starts_with('[') {
        let close = host_port.find(']').ok_or_else(invalid_url)?;
        match &host_port[close + 1..] {
            "" => None,
            suffix if suffix.starts_with(':') => Some(&suffix[1..]),
            _ => return Err(invalid_url()),
        }
    } else {
        host_port.rsplit_once(':').map(|(_, port)| port)
    };
    if let Some(port) = port {
        if port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.len() > 1 && port.starts_with('0')
            || port.parse::<u16>().ok().filter(|port| *port != 0).is_none()
        {
            return Err(invalid_url());
        }
    }
    Ok(())
}

fn has_invalid_percent_encoding(value: &[u8]) -> bool {
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'%' {
            if index + 2 >= value.len()
                || !value[index + 1].is_ascii_hexdigit()
                || !value[index + 2].is_ascii_hexdigit()
            {
                return true;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    false
}

fn validate_https_url(url: &Url, maximum: usize) -> Result<(), HttpError> {
    if url.as_str().len() > maximum
        || url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
        || url.port_or_known_default().is_none()
    {
        return Err(invalid_url());
    }
    Ok(())
}

/// Deny every non-global or ambiguity-prone address class used by the broker.
#[must_use]
pub fn is_denied_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_denied_v4(address),
        IpAddr::V6(address) => is_denied_v6(address),
    }
}

fn is_denied_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224
}

fn is_denied_v6(address: Ipv6Addr) -> bool {
    let bits = u128::from(address);
    // Only native global unicast is accepted. This rejects unspecified,
    // loopback, mapped/translated IPv4, NAT64, ULA, link-local, site-local,
    // multicast, and all other special-purpose prefixes by default.
    if !prefix(bits, 0x2000_u128 << 112, 3) {
        return true;
    }
    prefix(
        bits,
        u128::from(Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0)),
        32,
    ) || prefix(
        bits,
        u128::from(Ipv6Addr::new(0x2001, 0x0002, 0, 0, 0, 0, 0, 0)),
        48,
    ) || prefix(
        bits,
        u128::from(Ipv6Addr::new(0x2001, 0x0010, 0, 0, 0, 0, 0, 0)),
        28,
    ) || prefix(
        bits,
        u128::from(Ipv6Addr::new(0x2001, 0x0020, 0, 0, 0, 0, 0, 0)),
        28,
    ) || prefix(
        bits,
        u128::from(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0)),
        32,
    ) || prefix(
        bits,
        u128::from(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0)),
        16,
    ) || prefix(
        bits,
        u128::from(Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0)),
        20,
    )
}

fn prefix(value: u128, network: u128, length: u32) -> bool {
    let mask = u128::MAX << (128 - length);
    value & mask == network & mask
}

fn decode_body(
    headers: &BTreeMap<String, Vec<String>>,
    compressed: &[u8],
    limits: HttpLimits,
    decoded_usage: &mut u64,
) -> Result<Vec<u8>, HttpError> {
    let encodings = headers.get("content-encoding");
    let encoding = match encodings {
        None => "identity".to_owned(),
        Some(values) if values.len() == 1 && !values[0].contains(',') => {
            values[0].trim().to_ascii_lowercase()
        }
        Some(_) => return Err(unsupported_encoding()),
    };
    match encoding.as_str() {
        "identity" => {
            if compressed.len() as u64 > limits.max_decoded_bytes {
                return Err(decoded_limit());
            }
            charge_decoded_usage(decoded_usage, compressed.len())?;
            Ok(compressed.to_vec())
        }
        "gzip" => decode_gzip(compressed, limits, decoded_usage),
        _ => Err(unsupported_encoding()),
    }
}

fn decode_gzip(
    compressed: &[u8],
    limits: HttpLimits,
    decoded_usage: &mut u64,
) -> Result<Vec<u8>, HttpError> {
    let cursor = Cursor::new(compressed);
    let mut decoder = GzDecoder::new(cursor);
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8 * 1_024];
    loop {
        let read = decoder
            .read(&mut chunk)
            .map_err(|_| HttpError::new(HttpErrorCode::Protocol, "gzip decoding failed"))?;
        if read == 0 {
            break;
        }
        let next = output.len().checked_add(read).ok_or_else(decoded_limit)?;
        if next as u64 > limits.max_decoded_bytes {
            return Err(decoded_limit());
        }
        charge_decoded_usage(decoded_usage, read)?;
        if compressed.is_empty()
            || next as u128 > compressed.len() as u128 * u128::from(limits.max_decompression_ratio)
        {
            return Err(HttpError::new(
                HttpErrorCode::DecompressionRatio,
                "the decoded body exceeds its decompression ratio",
            ));
        }
        output.extend_from_slice(&chunk[..read]);
    }
    let consumed = decoder.into_inner().position();
    if consumed != compressed.len() as u64 {
        return Err(HttpError::new(
            HttpErrorCode::Protocol,
            "gzip body has trailing data",
        ));
    }
    Ok(output)
}

fn charge_decoded_usage(usage: &mut u64, bytes: usize) -> Result<(), HttpError> {
    let bytes = u64::try_from(bytes).map_err(|_| decoded_limit())?;
    *usage = usage.checked_add(bytes).ok_or_else(decoded_limit)?;
    Ok(())
}

type RawResponseHead = (u16, Vec<(String, Vec<u8>)>, Vec<u8>, usize);

fn read_response_head(
    stream: &mut StreamOwned<ClientConnection, TcpStream>,
    request: &TransportRequest,
    started: Instant,
) -> Result<RawResponseHead, HttpError> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4 * 1_024];
    let mut first = true;
    loop {
        let remaining = remaining_instant(started, request.total_timeout)?;
        let phase = if first {
            request.first_byte_timeout
        } else {
            request.idle_timeout
        };
        stream
            .sock
            .set_read_timeout(Some(phase.min(remaining)))
            .map_err(|_| io_error())?;
        let read = stream.read(&mut chunk).map_err(|error| {
            map_read_error(
                &error,
                if first {
                    HttpErrorCode::FirstByteTimeout
                } else {
                    HttpErrorCode::IdleTimeout
                },
            )
        })?;
        if read == 0 {
            return Err(protocol_error());
        }
        first = false;
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_header_end(&buffer) {
            if end > request.max_header_bytes {
                return Err(header_limit());
            }
            let mut slots = vec![httparse::EMPTY_HEADER; request.max_response_headers];
            let mut response = httparse::Response::new(&mut slots);
            let parsed = response.parse(&buffer[..end]).map_err(|error| {
                if error == httparse::Error::TooManyHeaders {
                    header_limit()
                } else {
                    protocol_error()
                }
            })?;
            if !parsed.is_complete() || response.version != Some(1) {
                return Err(protocol_error());
            }
            let status = response.code.ok_or_else(protocol_error)?;
            if status < 200 {
                return Err(protocol_error());
            }
            let headers = response
                .headers
                .iter()
                .map(|header| (header.name.to_owned(), header.value.to_vec()))
                .collect();
            return Ok((status, headers, buffer[end..].to_vec(), end));
        }
        if buffer.len() > request.max_header_bytes {
            return Err(header_limit());
        }
    }
}

fn read_response_body(
    stream: &mut StreamOwned<ClientConnection, TcpStream>,
    prefix_bytes: Vec<u8>,
    status: u16,
    headers: &[(String, Vec<u8>)],
    request: &TransportRequest,
    started: Instant,
) -> Result<Vec<u8>, HttpError> {
    let mut source = NetworkBody {
        prefix: prefix_bytes.into(),
        stream,
        started,
        total: request.total_timeout,
        idle: request.idle_timeout,
    };
    read_framed_body(&mut source, status, headers, request.max_compressed_bytes)
}

fn read_framed_body(
    source: &mut impl Read,
    status: u16,
    headers: &[(String, Vec<u8>)],
    maximum: u64,
) -> Result<Vec<u8>, HttpError> {
    if status == 204 || status == 304 {
        return Ok(Vec::new());
    }
    let content_lengths = header_values(headers, "content-length");
    let transfers = header_values(headers, "transfer-encoding");
    if !content_lengths.is_empty() && !transfers.is_empty() {
        return Err(protocol_error());
    }
    if !transfers.is_empty() {
        if transfers.len() != 1
            || !std::str::from_utf8(transfers[0])
                .is_ok_and(|value| value.trim().eq_ignore_ascii_case("chunked"))
        {
            return Err(protocol_error());
        }
        return read_chunked(source, maximum);
    }
    if !content_lengths.is_empty() {
        if content_lengths.len() != 1 {
            return Err(protocol_error());
        }
        let text = std::str::from_utf8(content_lengths[0]).map_err(|_| protocol_error())?;
        let length = text.trim().parse::<u64>().map_err(|_| protocol_error())?;
        if length > maximum {
            return Err(compressed_limit());
        }
        return read_exact_body(source, length);
    }
    read_until_close(source, maximum)
}

struct NetworkBody<'a> {
    prefix: VecDeque<u8>,
    stream: &'a mut StreamOwned<ClientConnection, TcpStream>,
    started: Instant,
    total: Duration,
    idle: Duration,
}

impl Read for NetworkBody<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if !self.prefix.is_empty() {
            let count = output.len().min(self.prefix.len());
            for byte in &mut output[..count] {
                *byte = self.prefix.pop_front().expect("prefix length was checked");
            }
            return Ok(count);
        }
        let remaining = self
            .total
            .checked_sub(self.started.elapsed())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "total deadline"))?;
        self.stream
            .sock
            .set_read_timeout(Some(self.idle.min(remaining)))?;
        self.stream.read(output)
    }
}

fn read_exact_body(reader: &mut impl Read, length: u64) -> Result<Vec<u8>, HttpError> {
    let capacity = usize::try_from(length).map_err(|_| compressed_limit())?;
    let mut output = vec![0_u8; capacity];
    reader
        .read_exact(&mut output)
        .map_err(|error| map_read_error(&error, HttpErrorCode::IdleTimeout))?;
    Ok(output)
}

fn read_until_close(reader: &mut impl Read, maximum: u64) -> Result<Vec<u8>, HttpError> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8 * 1_024];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(map_read_error(&error, HttpErrorCode::IdleTimeout)),
        };
        let next = output
            .len()
            .checked_add(read)
            .ok_or_else(compressed_limit)?;
        if next as u64 > maximum {
            return Err(compressed_limit());
        }
        output.extend_from_slice(&chunk[..read]);
    }
    Ok(output)
}

fn read_chunked(reader: &mut impl Read, maximum: u64) -> Result<Vec<u8>, HttpError> {
    let mut output = Vec::new();
    loop {
        let line = read_crlf_line(reader, MAX_CHUNK_LINE_BYTES)?;
        if line.contains(&b';') {
            return Err(protocol_error());
        }
        let text = std::str::from_utf8(&line).map_err(|_| protocol_error())?;
        let size = u64::from_str_radix(text.trim(), 16).map_err(|_| protocol_error())?;
        if size == 0 {
            if !read_crlf_line(reader, MAX_CHUNK_LINE_BYTES)?.is_empty() {
                return Err(protocol_error());
            }
            return Ok(output);
        }
        let next = (output.len() as u64)
            .checked_add(size)
            .ok_or_else(compressed_limit)?;
        if next > maximum {
            return Err(compressed_limit());
        }
        let mut chunk = read_exact_body(reader, size)?;
        output.append(&mut chunk);
        if !read_crlf_line(reader, 0)?.is_empty() {
            return Err(protocol_error());
        }
    }
}

fn read_crlf_line(reader: &mut impl Read, maximum: usize) -> Result<Vec<u8>, HttpError> {
    let mut output = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        reader
            .read_exact(&mut byte)
            .map_err(|error| map_read_error(&error, HttpErrorCode::IdleTimeout))?;
        output.push(byte[0]);
        if output.ends_with(b"\r\n") {
            output.truncate(output.len() - 2);
            return Ok(output);
        }
        if output.len() > maximum.saturating_add(2) {
            return Err(protocol_error());
        }
    }
}

fn header_values<'a>(headers: &'a [(String, Vec<u8>)], name: &str) -> Vec<&'a [u8]> {
    headers
        .iter()
        .filter_map(|(candidate, value)| {
            candidate
                .eq_ignore_ascii_case(name)
                .then_some(value.as_slice())
        })
        .collect()
}

fn request_target(url: &Url) -> String {
    let mut target = url.path().to_owned();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }
    target
}

fn request_authority(url: &Url) -> Result<String, HttpError> {
    let host = url.host_str().ok_or_else(invalid_url)?;
    let host = if matches!(url.host(), Some(Host::Ipv6(_))) {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn find_header_end(value: &[u8]) -> Option<usize> {
    value
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn remaining_instant(started: Instant, total: Duration) -> Result<Duration, HttpError> {
    total
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(total_timeout)
}

fn valid_header_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

const fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

fn map_connect_error(error: &io::Error) -> HttpError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        HttpError::new(
            HttpErrorCode::ConnectTimeout,
            "the connection exceeded its deadline",
        )
    } else {
        io_error()
    }
}

fn map_tls_io(error: &io::Error, timeout: HttpErrorCode) -> HttpError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        HttpError::new(timeout, "the network operation exceeded its deadline")
    } else {
        HttpError::new(HttpErrorCode::Tls, "TLS negotiation failed")
    }
}

fn map_read_error(error: &io::Error, timeout: HttpErrorCode) -> HttpError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        HttpError::new(timeout, "the response exceeded its deadline")
    } else if error.kind() == io::ErrorKind::UnexpectedEof {
        protocol_error()
    } else {
        io_error()
    }
}

const fn invalid_url() -> HttpError {
    HttpError::new(
        HttpErrorCode::InvalidUrl,
        "the URL is not an allowed absolute HTTPS URL",
    )
}
const fn peer_mismatch() -> HttpError {
    HttpError::new(
        HttpErrorCode::PeerMismatch,
        "the connected peer does not match the selected address",
    )
}
const fn protocol_error() -> HttpError {
    HttpError::new(HttpErrorCode::Protocol, "the HTTP response is malformed")
}
const fn io_error() -> HttpError {
    HttpError::new(HttpErrorCode::Io, "the network operation failed")
}
const fn header_limit() -> HttpError {
    HttpError::new(
        HttpErrorCode::HeaderLimit,
        "the response headers exceed their byte limit",
    )
}
const fn request_limit() -> HttpError {
    HttpError::new(
        HttpErrorCode::RequestLimit,
        "the HTTP request limit is exhausted",
    )
}
const fn redirect_limit() -> HttpError {
    HttpError::new(
        HttpErrorCode::RedirectLimit,
        "the HTTP redirect limit is exhausted",
    )
}
const fn redirect_invalid() -> HttpError {
    HttpError::new(
        HttpErrorCode::RedirectInvalid,
        "the redirect target is invalid",
    )
}
const fn compressed_limit() -> HttpError {
    HttpError::new(
        HttpErrorCode::CompressedLimit,
        "the compressed response exceeds its byte limit",
    )
}
const fn decoded_limit() -> HttpError {
    HttpError::new(
        HttpErrorCode::DecodedLimit,
        "the decoded response exceeds its byte limit",
    )
}
const fn unsupported_encoding() -> HttpError {
    HttpError::new(
        HttpErrorCode::UnsupportedEncoding,
        "the content encoding is not supported",
    )
}
const fn total_timeout() -> HttpError {
    HttpError::new(
        HttpErrorCode::TotalTimeout,
        "the HTTP call exceeded its total deadline",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    const PUBLIC_V4: Ipv4Addr = Ipv4Addr::new(93, 184, 216, 34);

    #[derive(Clone, Debug, Default)]
    struct ManualClock(Arc<AtomicU64>);

    impl ManualClock {
        fn advance(&self, duration: Duration) {
            self.0.fetch_add(
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::Relaxed))
        }
    }

    struct FakeResolver {
        clock: ManualClock,
        elapsed: Duration,
        answers: Mutex<VecDeque<Result<Vec<SocketAddr>, HttpError>>>,
        calls: Arc<Mutex<Vec<(String, u16, Duration)>>>,
    }

    impl Resolver for FakeResolver {
        fn resolve(
            &self,
            host: &str,
            port: u16,
            timeout: Duration,
        ) -> Result<Vec<SocketAddr>, HttpError> {
            self.calls
                .lock()
                .expect("resolver calls")
                .push((host.to_owned(), port, timeout));
            self.clock.advance(self.elapsed);
            self.answers
                .lock()
                .expect("resolver answers")
                .pop_front()
                .expect("resolver fixture has an answer")
        }
    }

    #[derive(Clone)]
    struct FakeReply {
        status: u16,
        headers: Vec<(String, Vec<u8>)>,
        body: Vec<u8>,
        peer: Option<SocketAddr>,
        elapsed: Duration,
    }

    struct FakeTransport {
        clock: ManualClock,
        replies: Mutex<VecDeque<Result<FakeReply, HttpError>>>,
        calls: Arc<Mutex<Vec<TransportRequest>>>,
    }

    impl Transport for FakeTransport {
        fn get(&self, request: &TransportRequest) -> Result<TransportResponse, HttpError> {
            self.calls
                .lock()
                .expect("transport calls")
                .push(request.clone());
            let reply = self
                .replies
                .lock()
                .expect("transport replies")
                .pop_front()
                .expect("transport fixture has a reply")?;
            self.clock.advance(reply.elapsed);
            let header_bytes = reply.headers.iter().fold(0_usize, |bytes, (name, value)| {
                bytes.saturating_add(name.len().saturating_add(value.len()).saturating_add(4))
            });
            Ok(TransportResponse {
                status: reply.status,
                headers: reply.headers,
                header_bytes,
                compressed_body: reply.body,
                peer_address: reply.peer.unwrap_or(request.selected_address),
            })
        }
    }

    struct Fixture {
        broker: HttpBroker,
        resolver_calls: Arc<Mutex<Vec<(String, u16, Duration)>>>,
        transport_calls: Arc<Mutex<Vec<TransportRequest>>>,
        clock: ManualClock,
    }

    fn make_fixture(
        origins: &[&str],
        answers: Vec<Result<Vec<SocketAddr>, HttpError>>,
        replies: Vec<Result<FakeReply, HttpError>>,
        limits: HttpLimits,
    ) -> Fixture {
        fixture_with_policy(
            origins,
            BTreeSet::new(),
            answers,
            replies,
            limits,
            Duration::ZERO,
        )
    }

    fn fixture_with_policy(
        origins: &[&str],
        denied_addresses: BTreeSet<IpAddr>,
        answers: Vec<Result<Vec<SocketAddr>, HttpError>>,
        replies: Vec<Result<FakeReply, HttpError>>,
        limits: HttpLimits,
        resolver_elapsed: Duration,
    ) -> Fixture {
        let clock = ManualClock::default();
        let resolver_calls = Arc::new(Mutex::new(Vec::new()));
        let transport_calls = Arc::new(Mutex::new(Vec::new()));
        let resolver = FakeResolver {
            clock: clock.clone(),
            elapsed: resolver_elapsed,
            answers: Mutex::new(answers.into()),
            calls: Arc::clone(&resolver_calls),
        };
        let transport = FakeTransport {
            clock: clock.clone(),
            replies: Mutex::new(replies.into()),
            calls: Arc::clone(&transport_calls),
        };
        let broker = HttpBroker::new_with_denied_addresses(
            origins.iter().map(|value| (*value).to_owned()),
            denied_addresses,
            limits,
            Box::new(resolver),
            Box::new(transport),
            Box::new(clock.clone()),
        )
        .expect("fixture broker");
        Fixture {
            broker,
            resolver_calls,
            transport_calls,
            clock,
        }
    }

    fn address(ip: IpAddr) -> SocketAddr {
        SocketAddr::new(ip, 443)
    }

    fn public_answer() -> Vec<SocketAddr> {
        vec![address(IpAddr::V4(PUBLIC_V4))]
    }

    fn reply(status: u16, body: &[u8]) -> FakeReply {
        FakeReply {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
            peer: None,
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn canonical_origins_are_https_only_and_exact() {
        assert_eq!(
            CanonicalOrigin::parse("https://example.com")
                .unwrap()
                .as_str(),
            "https://example.com"
        );
        assert_eq!(
            CanonicalOrigin::parse("https://xn--bcher-kva.example:8443")
                .unwrap()
                .as_str(),
            "https://xn--bcher-kva.example:8443"
        );
        assert_eq!(
            CanonicalOrigin::parse("https://[2606:4700:4700::1111]")
                .unwrap()
                .as_str(),
            "https://[2606:4700:4700::1111]"
        );
        for denied in [
            "http://example.com",
            "https://user@example.com",
            "https://example.com/path",
            "https://example.com?query",
            "https://example.com#fragment",
            "https://EXAMPLE.com",
            "https://example.com/",
        ] {
            assert_eq!(
                CanonicalOrigin::parse(denied).unwrap_err().code,
                HttpErrorCode::InvalidUrl,
                "{denied}"
            );
        }
    }

    #[test]
    fn defaults_match_the_current_contract() {
        let limits = HttpLimits::default();
        assert_eq!(limits.max_requests, 100);
        assert_eq!(limits.max_redirects, 10);
        assert_eq!(limits.max_dns_candidates, 32);
        assert_eq!(limits.max_response_headers, 256);
        assert_eq!(limits.max_header_bytes, 64 * 1_024);
        assert_eq!(limits.max_compressed_bytes, 16 * 1_024 * 1_024);
        assert_eq!(limits.max_decoded_bytes, 16 * 1_024 * 1_024);
        assert_eq!(limits.max_decompression_ratio, 100);
        assert_eq!(limits.connect_timeout, Duration::from_secs(10));
        assert_eq!(limits.first_byte_timeout, Duration::from_secs(10));
        assert_eq!(limits.idle_timeout, Duration::from_secs(10));
        assert_eq!(limits.total_timeout, Duration::from_secs(30));
    }

    #[test]
    fn destination_classifier_denies_special_ranges() {
        for denied in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.0.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::7f00:1",
            "100::1",
            "2001::1",
            "2001:2::1",
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "fc00::1",
            "fe80::1",
            "ff00::1",
        ] {
            let address = denied.parse().unwrap();
            assert!(is_denied_address(address), "{denied}");
        }
        for allowed in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            let address = allowed.parse().unwrap();
            assert!(!is_denied_address(address), "{allowed}");
        }
    }

    #[test]
    fn mixed_dns_answer_is_rejected_before_connect() {
        let mut origin_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(vec![
                address(IpAddr::V4(PUBLIC_V4)),
                address(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            ])],
            Vec::new(),
            HttpLimits::default(),
        );
        assert_eq!(
            origin_fixture
                .broker
                .get("https://example.com/data")
                .unwrap_err()
                .code,
            HttpErrorCode::DestinationDenied
        );
        assert!(origin_fixture.transport_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn host_denials_and_dns_candidate_limits_apply_to_the_whole_answer() {
        let denied = BTreeSet::from([IpAddr::V4(PUBLIC_V4)]);
        let mut denied_fixture = fixture_with_policy(
            &["https://example.com"],
            denied,
            vec![Ok(public_answer())],
            Vec::new(),
            HttpLimits::default(),
            Duration::ZERO,
        );
        assert_eq!(
            denied_fixture
                .broker
                .get("https://example.com/data")
                .unwrap_err()
                .code,
            HttpErrorCode::DestinationDenied
        );
        assert!(denied_fixture.transport_calls.lock().unwrap().is_empty());

        let mut denied_literal = fixture_with_policy(
            &["https://93.184.216.34"],
            BTreeSet::from([IpAddr::V4(PUBLIC_V4)]),
            Vec::new(),
            Vec::new(),
            HttpLimits::default(),
            Duration::ZERO,
        );
        assert_eq!(
            denied_literal
                .broker
                .get("https://93.184.216.34/data")
                .unwrap_err()
                .code,
            HttpErrorCode::DestinationDenied
        );
        assert!(denied_literal.resolver_calls.lock().unwrap().is_empty());

        let limits = HttpLimits {
            max_dns_candidates: 1,
            ..HttpLimits::default()
        };
        let mut oversized_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(vec![
                address(IpAddr::V4(PUBLIC_V4)),
                address(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))),
            ])],
            Vec::new(),
            limits,
        );
        assert_eq!(
            oversized_fixture
                .broker
                .get("https://example.com/data")
                .unwrap_err()
                .code,
            HttpErrorCode::Dns
        );
    }

    #[test]
    fn non_2xx_response_is_success_and_headers_are_normalized() {
        let mut response = reply(404, b"missing");
        response.headers = vec![
            ("X-Test".to_owned(), b"one".to_vec()),
            ("x-test".to_owned(), b"two".to_vec()),
        ];
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(response)],
            HttpLimits::default(),
        );
        let response = fixture.broker.get("https://example.com/data?x=1").unwrap();
        assert_eq!(response.status, 404);
        assert_eq!(response.body, b"missing");
        assert_eq!(response.headers["x-test"], ["one", "two"]);
        assert_eq!(response.final_url, "https://example.com/data?x=1");
    }

    #[test]
    fn request_surface_is_url_only_with_no_ambient_or_caller_credentials() {
        let get: fn(&mut HttpBroker, &str) -> Result<HttpResponse, HttpError> = HttpBroker::get;
        let _ = get;
        let RustlsTransport { tls: _ } = RustlsTransport::default();

        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(reply(200, b"ok"))],
            HttpLimits::default(),
        );
        fixture.broker.get("https://example.com/data").unwrap();
        let request = fixture.transport_calls.lock().unwrap()[0].clone();
        assert_eq!(request.user_agent(), USER_AGENT);
        assert_eq!(request.accept_encoding(), "gzip");
        let TransportRequest {
            url,
            selected_address,
            max_response_headers: _,
            max_header_bytes: _,
            max_compressed_bytes: _,
            connect_timeout: _,
            first_byte_timeout: _,
            idle_timeout: _,
            total_timeout: _,
        } = request;
        assert_eq!(url.as_str(), "https://example.com/data");
        assert_eq!(selected_address.ip(), IpAddr::V4(PUBLIC_V4));
    }

    #[test]
    fn relative_and_absolute_redirects_are_revalidated_per_hop() {
        let mut relative = reply(302, b"ignored");
        relative
            .headers
            .push(("Location".to_owned(), b"next".to_vec()));
        let mut absolute = reply(307, b"ignored");
        absolute.headers.push((
            "Location".to_owned(),
            b"https://example.com/final?x=1".to_vec(),
        ));
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![
                Ok(public_answer()),
                Ok(public_answer()),
                Ok(public_answer()),
            ],
            vec![Ok(relative), Ok(absolute), Ok(reply(200, b"done"))],
            HttpLimits::default(),
        );
        let response = fixture
            .broker
            .get("https://example.com/base/start")
            .unwrap();
        assert_eq!(response.final_url, "https://example.com/final?x=1");
        assert_eq!(response.body, b"done");
        assert_eq!(fixture.broker.usage().requests, 3);
        assert_eq!(fixture.broker.usage().redirects, 2);
        assert_eq!(fixture.resolver_calls.lock().unwrap().len(), 3);
        let calls = fixture.transport_calls.lock().unwrap();
        assert_eq!(calls[1].url.as_str(), "https://example.com/base/next");
        assert_eq!(calls[2].url.as_str(), "https://example.com/final?x=1");
    }

    #[test]
    fn redirect_cannot_escape_the_effective_origin_set() {
        let mut redirect = reply(301, b"");
        redirect.headers.push((
            "location".to_owned(),
            b"https://other.example/data".to_vec(),
        ));
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(redirect)],
            HttpLimits::default(),
        );
        assert_eq!(
            fixture
                .broker
                .get("https://example.com/start")
                .unwrap_err()
                .code,
            HttpErrorCode::OriginDenied
        );
        assert_eq!(fixture.transport_calls.lock().unwrap().len(), 1);

        let mut redirect = reply(301, b"");
        redirect.headers.push((
            "location".to_owned(),
            b"https://other.example/data".to_vec(),
        ));
        let mut fixture = make_fixture(
            &["https://example.com", "https://other.example"],
            vec![
                Ok(public_answer()),
                Ok(vec![address(IpAddr::V4(Ipv4Addr::LOCALHOST))]),
            ],
            vec![Ok(redirect)],
            HttpLimits::default(),
        );
        assert_eq!(
            fixture
                .broker
                .get("https://example.com/start")
                .unwrap_err()
                .code,
            HttpErrorCode::DestinationDenied
        );
        assert_eq!(fixture.transport_calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn redirect_rejects_downgrade_and_malformed_locations() {
        for location in [
            "http://example.com/insecure",
            "https://example.com/%zz",
            "https://example.com:",
        ] {
            let mut redirect = reply(302, b"");
            redirect
                .headers
                .push(("location".to_owned(), location.as_bytes().to_vec()));
            let mut fixture = make_fixture(
                &["https://example.com"],
                vec![Ok(public_answer())],
                vec![Ok(redirect)],
                HttpLimits::default(),
            );
            assert_eq!(
                fixture
                    .broker
                    .get("https://example.com/start")
                    .unwrap_err()
                    .code,
                HttpErrorCode::RedirectInvalid,
                "{location}"
            );
            assert_eq!(fixture.transport_calls.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn redirect_cycle_stops_at_the_count_limit() {
        let redirect = |location: &str| {
            let mut response = reply(302, b"");
            response
                .headers
                .push(("location".to_owned(), location.as_bytes().to_vec()));
            Ok(response)
        };
        let limits = HttpLimits {
            max_redirects: 2,
            ..HttpLimits::default()
        };
        let mut cycle_fixture = make_fixture(
            &["https://example.com"],
            vec![
                Ok(public_answer()),
                Ok(public_answer()),
                Ok(public_answer()),
            ],
            vec![redirect("/b"), redirect("/a"), redirect("/b")],
            limits,
        );
        assert_eq!(
            cycle_fixture
                .broker
                .get("https://example.com/a")
                .unwrap_err()
                .code,
            HttpErrorCode::RedirectLimit
        );
        assert_eq!(cycle_fixture.broker.usage().requests, 3);
        assert_eq!(cycle_fixture.broker.usage().redirects, 2);
    }

    #[test]
    fn connected_peer_must_equal_the_selected_address() {
        let mut mismatch = reply(200, b"");
        mismatch.peer = Some(address(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(mismatch)],
            HttpLimits::default(),
        );
        assert_eq!(
            fixture.broker.get("https://example.com").unwrap_err().code,
            HttpErrorCode::PeerMismatch
        );
    }

    #[test]
    fn gzip_is_the_only_decoded_content_encoding() {
        let mut compressor = GzEncoder::new(Vec::new(), Compression::default());
        compressor.write_all(b"decoded").unwrap();
        let encoded = compressor.finish().unwrap();
        let mut gzip = reply(200, &encoded);
        gzip.headers
            .push(("Content-Encoding".to_owned(), b"gzip".to_vec()));
        let mut gzip_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(gzip)],
            HttpLimits::default(),
        );
        assert_eq!(
            gzip_fixture.broker.get("https://example.com").unwrap().body,
            b"decoded"
        );

        let mut unsupported = reply(200, b"value");
        unsupported
            .headers
            .push(("content-encoding".to_owned(), b"br".to_vec()));
        let mut ratio_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(unsupported)],
            HttpLimits::default(),
        );
        assert_eq!(
            ratio_fixture
                .broker
                .get("https://example.com")
                .unwrap_err()
                .code,
            HttpErrorCode::UnsupportedEncoding
        );

        let mut nested = reply(200, &encoded);
        nested
            .headers
            .push(("content-encoding".to_owned(), b"gzip, gzip".to_vec()));
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(nested)],
            HttpLimits::default(),
        );
        assert_eq!(
            fixture.broker.get("https://example.com").unwrap_err().code,
            HttpErrorCode::UnsupportedEncoding
        );
    }

    #[test]
    fn decompression_ratio_and_decoded_size_are_bounded() {
        let value = vec![b'a'; 64 * 1_024];
        let mut compressor = GzEncoder::new(Vec::new(), Compression::best());
        compressor.write_all(&value).unwrap();
        let encoded = compressor.finish().unwrap();
        let mut response = reply(200, &encoded);
        response
            .headers
            .push(("content-encoding".to_owned(), b"gzip".to_vec()));
        let limits = HttpLimits {
            max_decompression_ratio: 2,
            ..HttpLimits::default()
        };
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(response)],
            limits,
        );
        assert_eq!(
            fixture.broker.get("https://example.com").unwrap_err().code,
            HttpErrorCode::DecompressionRatio
        );

        let decoded_limits = HttpLimits {
            max_decoded_bytes: 1_024,
            max_decompression_ratio: 1_000,
            ..HttpLimits::default()
        };
        let mut decoded_response = reply(200, &encoded);
        decoded_response
            .headers
            .push(("content-encoding".to_owned(), b"gzip".to_vec()));
        let mut decoded_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(decoded_response)],
            decoded_limits,
        );
        assert_eq!(
            decoded_fixture
                .broker
                .get("https://example.com")
                .unwrap_err()
                .code,
            HttpErrorCode::DecodedLimit
        );

        let mut truncated = encoded;
        truncated.truncate(truncated.len().saturating_sub(4));
        let mut truncated_response = reply(200, &truncated);
        truncated_response
            .headers
            .push(("content-encoding".to_owned(), b"gzip".to_vec()));
        let mut truncated_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(truncated_response)],
            HttpLimits {
                max_decompression_ratio: 1_000,
                ..HttpLimits::default()
            },
        );
        assert_eq!(
            truncated_fixture
                .broker
                .get("https://example.com")
                .unwrap_err()
                .code,
            HttpErrorCode::Protocol
        );
    }

    #[test]
    fn stage_timeout_surfaces_are_stable_and_network_free() {
        let timed_out = io::Error::new(io::ErrorKind::TimedOut, "fixture deadline");
        assert_eq!(
            map_connect_error(&timed_out).code,
            HttpErrorCode::ConnectTimeout
        );
        assert_eq!(
            map_read_error(&timed_out, HttpErrorCode::FirstByteTimeout).code,
            HttpErrorCode::FirstByteTimeout
        );
        assert_eq!(
            map_read_error(&timed_out, HttpErrorCode::IdleTimeout).code,
            HttpErrorCode::IdleTimeout
        );

        let mut dns_fixture = fixture_with_policy(
            &["https://example.com"],
            BTreeSet::new(),
            vec![Ok(public_answer())],
            Vec::new(),
            HttpLimits::default(),
            Duration::from_secs(6),
        );
        assert_eq!(
            dns_fixture
                .broker
                .get("https://example.com")
                .unwrap_err()
                .code,
            HttpErrorCode::DnsTimeout
        );
        assert!(dns_fixture.transport_calls.lock().unwrap().is_empty());

        for code in [
            HttpErrorCode::ConnectTimeout,
            HttpErrorCode::FirstByteTimeout,
            HttpErrorCode::IdleTimeout,
        ] {
            let mut fixture = make_fixture(
                &["https://example.com"],
                vec![Ok(public_answer())],
                vec![Err(HttpError::new(code, "fixture deadline"))],
                HttpLimits::default(),
            );
            assert_eq!(
                fixture.broker.get("https://example.com").unwrap_err().code,
                code
            );
        }
    }

    #[test]
    fn total_deadline_is_checked_after_transport_completion() {
        let mut delayed = reply(200, b"late");
        delayed.elapsed = Duration::from_secs(31);
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(delayed)],
            HttpLimits::default(),
        );
        assert_eq!(
            fixture.broker.get("https://example.com").unwrap_err().code,
            HttpErrorCode::TotalTimeout
        );
        assert_eq!(fixture.clock.now(), Duration::from_secs(31));
    }

    #[test]
    fn invalid_urls_fail_before_resolution_or_transport() {
        for denied in [
            "/relative",
            "http://example.com",
            "file:///tmp/value",
            "https://user:secret@example.com",
            "https://example.com/#fragment",
            "https://example.com/%",
            "https://example.com/%zz",
            "https://%65xample.com/",
            "https://example.com:",
            "https://example.com:0443/",
            "https://example.com:0/",
            "https://example.com:65536/",
            "https://example.com\\path",
            "https://xn--/",
        ] {
            let mut fixture = make_fixture(
                &["https://example.com"],
                Vec::new(),
                Vec::new(),
                HttpLimits::default(),
            );
            assert_eq!(
                fixture.broker.get(denied).unwrap_err().code,
                HttpErrorCode::InvalidUrl,
                "{denied}"
            );
            assert!(fixture.resolver_calls.lock().unwrap().is_empty());
            assert!(fixture.transport_calls.lock().unwrap().is_empty());
        }

        let limits = HttpLimits {
            max_url_bytes: 24,
            ..HttpLimits::default()
        };
        let mut length_fixture =
            make_fixture(&["https://example.com"], Vec::new(), Vec::new(), limits);
        assert_eq!(
            length_fixture
                .broker
                .get("https://example.com/too-long")
                .unwrap_err()
                .code,
            HttpErrorCode::InvalidUrl
        );

        let mut undeclared_fixture = make_fixture(
            &["https://example.com"],
            Vec::new(),
            Vec::new(),
            HttpLimits::default(),
        );
        assert_eq!(
            undeclared_fixture
                .broker
                .get("https://other.example/data")
                .unwrap_err()
                .code,
            HttpErrorCode::OriginDenied
        );
        assert!(undeclared_fixture.resolver_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn direct_public_ip_skips_name_resolution() {
        let mut fixture = make_fixture(
            &["https://93.184.216.34"],
            Vec::new(),
            vec![Ok(reply(200, b"direct"))],
            HttpLimits::default(),
        );
        assert_eq!(
            fixture
                .broker
                .get("https://93.184.216.34/value")
                .unwrap()
                .body,
            b"direct"
        );
        assert!(fixture.resolver_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn request_and_redirect_counts_are_hard_execution_limits() {
        let request_limits = HttpLimits {
            max_requests: 1,
            ..HttpLimits::default()
        };
        let mut request_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(reply(200, b"first"))],
            request_limits,
        );
        request_fixture
            .broker
            .get("https://example.com/first")
            .unwrap();
        assert_eq!(
            request_fixture
                .broker
                .get("https://example.com/second")
                .unwrap_err()
                .code,
            HttpErrorCode::RequestLimit
        );

        let redirect_limits = HttpLimits {
            max_redirects: 0,
            ..HttpLimits::default()
        };
        let mut redirect = reply(302, b"");
        redirect
            .headers
            .push(("location".to_owned(), b"/final".to_vec()));
        let mut redirect_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(redirect)],
            redirect_limits,
        );
        assert_eq!(
            redirect_fixture
                .broker
                .get("https://example.com/start")
                .unwrap_err()
                .code,
            HttpErrorCode::RedirectLimit
        );
    }

    #[test]
    fn response_headers_are_charged_cumulatively() {
        let limits = HttpLimits {
            max_header_bytes: 10,
            ..HttpLimits::default()
        };
        let mut first = reply(200, b"");
        first.headers.push(("x".to_owned(), b"a".to_vec()));
        let mut second = reply(200, b"");
        second.headers.push(("y".to_owned(), b"b".to_vec()));
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer()), Ok(public_answer())],
            vec![Ok(first), Ok(second)],
            limits,
        );
        fixture.broker.get("https://example.com/one").unwrap();
        assert_eq!(
            fixture
                .broker
                .get("https://example.com/two")
                .unwrap_err()
                .code,
            HttpErrorCode::HeaderLimit
        );
        assert_eq!(fixture.broker.usage().header_bytes, 6);
        let calls = fixture.transport_calls.lock().unwrap();
        assert_eq!(calls[1].max_header_bytes, 4);
    }

    #[test]
    fn response_header_count_is_nonzero_and_enforced_per_hop() {
        let invalid = HttpLimits {
            max_response_headers: 0,
            ..HttpLimits::default()
        };
        assert_eq!(
            invalid.validate().unwrap_err().code,
            HttpErrorCode::InvalidLimits
        );

        let limits = HttpLimits {
            max_response_headers: 1,
            ..HttpLimits::default()
        };
        let mut first = reply(200, b"");
        first.headers.push(("x-first".to_owned(), b"one".to_vec()));
        let mut second = reply(200, b"");
        second
            .headers
            .push(("x-second".to_owned(), b"two".to_vec()));
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer()), Ok(public_answer())],
            vec![Ok(first), Ok(second)],
            limits,
        );
        fixture.broker.get("https://example.com/one").unwrap();
        assert_eq!(
            fixture
                .broker
                .get("https://example.com/two")
                .unwrap_err()
                .code,
            HttpErrorCode::HeaderLimit
        );
        let calls = fixture.transport_calls.lock().unwrap();
        assert_eq!(calls[0].max_response_headers, 1);
        assert_eq!(calls[1].max_response_headers, 0);
        assert_eq!(fixture.broker.usage().response_headers, 1);
    }

    #[test]
    fn content_length_and_chunked_framing_are_bounded_and_strict() {
        let content_length = vec![("content-length".to_owned(), b"4".to_vec())];
        assert_eq!(
            read_framed_body(&mut Cursor::new(b"bodyextra"), 200, &content_length, 4).unwrap(),
            b"body"
        );
        assert_eq!(
            read_framed_body(&mut Cursor::new(b"body"), 200, &content_length, 3)
                .unwrap_err()
                .code,
            HttpErrorCode::CompressedLimit
        );
        assert_eq!(
            read_framed_body(&mut Cursor::new(b"bod"), 200, &content_length, 4)
                .unwrap_err()
                .code,
            HttpErrorCode::Protocol
        );

        let chunked = vec![("transfer-encoding".to_owned(), b"chunked".to_vec())];
        let chunks = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(
            read_framed_body(&mut Cursor::new(chunks), 200, &chunked, 9).unwrap(),
            b"Wikipedia"
        );
        assert_eq!(
            read_framed_body(&mut Cursor::new(chunks), 200, &chunked, 8)
                .unwrap_err()
                .code,
            HttpErrorCode::CompressedLimit
        );
        assert_eq!(
            read_framed_body(
                &mut Cursor::new(b"4;ext=1\r\nbody\r\n0\r\n\r\n"),
                200,
                &chunked,
                16,
            )
            .unwrap_err()
            .code,
            HttpErrorCode::Protocol
        );

        let ambiguous = vec![
            ("content-length".to_owned(), b"4".to_vec()),
            ("transfer-encoding".to_owned(), b"chunked".to_vec()),
        ];
        assert_eq!(
            read_framed_body(&mut Cursor::new(b"body"), 200, &ambiguous, 4)
                .unwrap_err()
                .code,
            HttpErrorCode::Protocol
        );
    }

    #[test]
    fn per_response_and_cumulative_body_limits_are_charged() {
        let limits = HttpLimits {
            max_compressed_bytes: 5,
            max_decoded_bytes: 5,
            ..HttpLimits::default()
        };
        let mut fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer())],
            vec![Ok(reply(200, b"123456"))],
            limits,
        );
        assert_eq!(
            fixture.broker.get("https://example.com").unwrap_err().code,
            HttpErrorCode::CompressedLimit
        );

        let limits = HttpLimits {
            max_compressed_bytes: 5,
            max_decoded_bytes: 10,
            ..HttpLimits::default()
        };
        let mut compressed_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer()), Ok(public_answer())],
            vec![Ok(reply(200, b"123")), Ok(reply(200, b"456"))],
            limits,
        );
        compressed_fixture
            .broker
            .get("https://example.com/one")
            .unwrap();
        assert_eq!(
            compressed_fixture
                .broker
                .get("https://example.com/two")
                .unwrap_err()
                .code,
            HttpErrorCode::CompressedLimit
        );
        let calls = compressed_fixture.transport_calls.lock().unwrap();
        assert_eq!(calls[1].max_compressed_bytes, 2);

        let limits = HttpLimits {
            max_compressed_bytes: 10,
            max_decoded_bytes: 5,
            ..HttpLimits::default()
        };
        let mut decoded_fixture = make_fixture(
            &["https://example.com"],
            vec![Ok(public_answer()), Ok(public_answer())],
            vec![Ok(reply(200, b"123")), Ok(reply(200, b"456"))],
            limits,
        );
        decoded_fixture
            .broker
            .get("https://example.com/one")
            .unwrap();
        assert_eq!(
            decoded_fixture
                .broker
                .get("https://example.com/two")
                .unwrap_err()
                .code,
            HttpErrorCode::DecodedLimit
        );
        assert_eq!(decoded_fixture.broker.usage().decoded_bytes, 3);
    }
}
