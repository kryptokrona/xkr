//! How requests reach the daemon: the [`Transport`] trait and its HTTP implementation
//! [`HttpTransport`] (hyper-util connection pool, keep-alive, timeouts, `http://` and
//! `https://`).
//!
//! The C++ wallets talk to the daemon through `httplib::Client` (`src/nigel/nigel.cpp`
//! `Nigel::Nigel`, a 10 s timeout, a new connection per request); this transport keeps
//! connections open between requests instead, which both xkrd and the C++ `HttpServer` allow.
//! Tests use their own [`Transport`] that hands requests to the `xkr-rpc` router in process.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::{Uri, header};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// HTTP method of a request. The C++ server ignores the method; Nigel uses `GET` for `/info`
/// and `/fee` and `POST` for everything else, and so does [`crate::DaemonClient`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET` (no body).
    Get,
    /// `POST` with a JSON body.
    Post,
}

/// One request to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// `GET` or `POST`.
    pub method: Method,
    /// Path on the daemon, starting with `/` (`/getinfo`, `/json_rpc`).
    pub path: String,
    /// JSON body (empty for `GET`).
    pub body: Vec<u8>,
}

/// The daemon's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body.
    pub body: Vec<u8>,
}

/// A request that got no answer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The daemon address does not parse or uses another scheme than `http` / `https`.
    #[error("invalid daemon address {0:?}")]
    Address(String),
    /// TLS could not be set up (no usable root certificates, bad configuration).
    #[error("TLS setup failed: {0}")]
    Tls(String),
    /// Connecting, sending or reading failed.
    #[error("request failed: {0}")]
    Connection(String),
    /// No complete answer within the request timeout.
    #[error("no answer within {0:?}")]
    Timeout(Duration),
    /// The answer is larger than [`HttpConfig::max_answer_bytes`].
    #[error("answer larger than {0} bytes")]
    TooLarge(usize),
}

/// Sends requests to a daemon. Implemented by [`HttpTransport`] and by test transports that
/// call a router directly.
pub trait Transport: Send + Sync {
    /// Sends `request` and returns the answer, whatever its HTTP status.
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, TransportError>> + Send;
}

/// Settings of [`HttpTransport`].
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Longest time one request may take, connecting and reading the whole answer included.
    /// Default 30 s (the xkrd request timeout; Nigel uses 10 s, too short for a full
    /// `/getwalletsyncdata` batch from a slow public node).
    pub request_timeout: Duration,
    /// Longest time to open the TCP connection. Default 10 s.
    pub connect_timeout: Duration,
    /// TCP keep-alive probe interval of open connections. Default 60 s.
    pub tcp_keepalive: Option<Duration>,
    /// Idle connections are closed after this long. Default 90 s.
    pub pool_idle_timeout: Duration,
    /// Most idle connections kept open. Default 4.
    pub pool_max_idle: usize,
    /// Largest answer accepted. Default 256 MiB (a `/gettransactions` answer can carry up to
    /// 32 MB of transactions as hex on xkrd).
    pub max_answer_bytes: usize,
    /// TLS settings for `https://` daemons; `None` uses the system root certificates with the
    /// ring provider (as `xkr-hugin` does for stratum pools).
    pub tls: Option<Arc<rustls::ClientConfig>>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            tcp_keepalive: Some(Duration::from_secs(60)),
            pool_idle_timeout: Duration::from_secs(90),
            pool_max_idle: 4,
            max_answer_bytes: 256 * 1024 * 1024,
            tls: None,
        }
    }
}

/// Where the daemon is: scheme, authority and an optional path prefix (a node behind a reverse
/// proxy at `https://example.org/node`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonUrl {
    https: bool,
    authority: String,
    prefix: String,
}

impl DaemonUrl {
    /// Parses `http://host:port`, `https://host[:port][/prefix]` or a bare `host:port` (plain
    /// HTTP, like the C++ `--daemon-address` / `--daemon-port` pair).
    pub fn parse(text: &str) -> Result<Self, TransportError> {
        let bad = || TransportError::Address(text.to_owned());
        let trimmed = text.trim();
        let full = if trimmed.contains("://") {
            trimmed.to_owned()
        } else {
            format!("http://{trimmed}")
        };
        let uri: Uri = full.parse().map_err(|_| bad())?;
        let https = match uri.scheme_str() {
            Some("http") => false,
            Some("https") => true,
            _ => return Err(bad()),
        };
        let authority = uri.authority().ok_or_else(bad)?;
        if authority.host().is_empty() || authority.as_str().contains('@') {
            return Err(bad());
        }
        if uri.query().is_some() {
            return Err(bad());
        }
        let prefix = uri.path().trim_end_matches('/').to_owned();
        Ok(Self {
            https,
            authority: authority.as_str().to_owned(),
            prefix,
        })
    }

    /// A plain-HTTP daemon at `host:port` (C++ `Nigel(daemonHost, daemonPort)`).
    pub fn from_host_port(host: &str, port: u16) -> Result<Self, TransportError> {
        let host = host.trim();
        if host.contains(':') && !host.starts_with('[') {
            Self::parse(&format!("[{host}]:{port}"))
        } else {
            Self::parse(&format!("{host}:{port}"))
        }
    }

    /// Whether the daemon is reached over TLS.
    #[must_use]
    pub fn is_https(&self) -> bool {
        self.https
    }

    /// `host:port` as given.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The full URI of `path` (which starts with `/`).
    fn uri(&self, path: &str) -> Result<Uri, TransportError> {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}{}{path}", self.authority, self.prefix)
            .parse()
            .map_err(|_| TransportError::Address(format!("{}{path}", self.authority)))
    }
}

impl std::fmt::Display for DaemonUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let scheme = if self.https { "https" } else { "http" };
        write!(f, "{scheme}://{}{}", self.authority, self.prefix)
    }
}

/// The HTTP [`Transport`]: a hyper-util client with a connection pool (keep-alive), TCP
/// keep-alive, a connect timeout, a whole-request timeout, an answer size limit and TLS for
/// `https://` daemons. Cheap to clone; clones share the pool.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: Client<Connector, Full<Bytes>>,
    url: DaemonUrl,
    request_timeout: Duration,
    max_answer_bytes: usize,
}

impl HttpTransport {
    /// A transport to `url` (see [`DaemonUrl::parse`]). Building it needs no runtime; requests
    /// are sent on the current tokio runtime.
    pub fn new(url: &str, config: &HttpConfig) -> Result<Self, TransportError> {
        Self::with_url(DaemonUrl::parse(url)?, config)
    }

    /// A transport to an already parsed address.
    pub fn with_url(url: DaemonUrl, config: &HttpConfig) -> Result<Self, TransportError> {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_nodelay(true);
        http.set_keepalive(config.tcp_keepalive);
        http.set_connect_timeout(Some(config.connect_timeout));
        let tls = if url.https {
            let cfg = match &config.tls {
                Some(cfg) => cfg.clone(),
                None => default_tls_config()?,
            };
            Some(TlsConnector::from(cfg))
        } else {
            None
        };
        let connector = Connector { http, tls };
        let client = Client::builder(TokioExecutor::new())
            .pool_timer(TokioTimer::new())
            .pool_idle_timeout(config.pool_idle_timeout)
            .pool_max_idle_per_host(config.pool_max_idle)
            .build(connector);
        Ok(Self {
            client,
            url,
            request_timeout: config.request_timeout,
            max_answer_bytes: config.max_answer_bytes,
        })
    }

    /// The daemon this transport talks to.
    #[must_use]
    pub fn url(&self) -> &DaemonUrl {
        &self.url
    }

    async fn exchange(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let uri = self.url.uri(&request.path)?;
        let method = match request.method {
            Method::Get => hyper::Method::GET,
            Method::Post => hyper::Method::POST,
        };
        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(uri)
            .header(header::ACCEPT, "application/json");
        if request.method == Method::Post {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let req = builder
            .body(Full::new(Bytes::from(request.body)))
            .map_err(|e| TransportError::Address(e.to_string()))?;
        let resp = self
            .client
            .request(req)
            .await
            .map_err(|e| TransportError::Connection(error_chain(&e)))?;
        let status = resp.status().as_u16();
        let limit = self.max_answer_bytes;
        let body = Limited::new(resp.into_body(), limit)
            .collect()
            .await
            .map_err(|e| {
                if e.is::<http_body_util::LengthLimitError>() {
                    TransportError::TooLarge(limit)
                } else {
                    TransportError::Connection(e.to_string())
                }
            })?
            .to_bytes();
        Ok(HttpResponse {
            status,
            body: body.to_vec(),
        })
    }
}

impl Transport for HttpTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let path = request.path.clone();
        let timeout = self.request_timeout;
        let Ok(result) = tokio::time::timeout(timeout, self.exchange(request)).await else {
            tracing::debug!(
                daemon = %self.url,
                path = %path,
                ?timeout,
                "daemon request timed out"
            );
            return Err(TransportError::Timeout(timeout));
        };
        if let Err(e) = &result {
            tracing::debug!(
                daemon = %self.url,
                path = %path,
                error = %e,
                "daemon request failed"
            );
        }
        result
    }
}

/// An error with its sources (hyper-util wraps the I/O cause).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

/// The ring provider, safe default protocol versions and the system root certificates (as
/// `xkr-stratum` `client::tls_connector`).
fn default_tls_config() -> Result<Arc<rustls::ClientConfig>, TransportError> {
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for e in &loaded.errors {
        tracing::debug!(error = %e, "loading a system root certificate failed");
    }
    let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        return Err(TransportError::Tls(
            "no system root certificates found".to_owned(),
        ));
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| TransportError::Tls(e.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// Opens TCP connections through hyper-util's [`HttpConnector`] (DNS, connect timeout,
/// keep-alive) and adds TLS for `https` URIs.
#[derive(Clone)]
struct Connector {
    http: HttpConnector,
    tls: Option<TlsConnector>,
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connector")
            .field("tls", &self.tls.is_some())
            .finish_non_exhaustive()
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl tower::Service<Uri> for Connector {
    type Response = TokioIo<MaybeTls>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tower::Service::<Uri>::poll_ready(&mut self.http, cx).map_err(Into::into)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let https = uri.scheme_str() == Some("https");
        let host = uri.host().unwrap_or_default().to_owned();
        let connecting = tower::Service::<Uri>::call(&mut self.http, uri);
        let tls = self.tls.clone();
        Box::pin(async move {
            let tcp = connecting.await?.into_inner();
            if !https {
                return Ok(TokioIo::new(MaybeTls::Plain(tcp)));
            }
            let tls = tls.ok_or("https address without TLS settings")?;
            let name = host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned();
            let name = ServerName::try_from(name)?;
            let stream = tls.connect(name, tcp).await?;
            Ok(TokioIo::new(MaybeTls::Tls(Box::new(stream))))
        })
    }
}

/// A plain or TLS connection.
#[derive(Debug)]
enum MaybeTls {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl Connection for MaybeTls {
    fn connected(&self) -> Connected {
        match self {
            Self::Plain(s) => s.connected(),
            Self::Tls(s) => s.get_ref().0.connected(),
        }
    }
}

impl AsyncRead for MaybeTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Plain(s) => s.is_write_vectored(),
            Self::Tls(s) => s.is_write_vectored(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_addresses_parse() {
        let u = DaemonUrl::parse("127.0.0.1:11898").unwrap();
        assert!(!u.is_https());
        assert_eq!(u.authority(), "127.0.0.1:11898");
        assert_eq!(u.to_string(), "http://127.0.0.1:11898");
        assert_eq!(
            u.uri("/getinfo").unwrap().to_string(),
            "http://127.0.0.1:11898/getinfo"
        );

        let u = DaemonUrl::parse("https://node.example.org/api/").unwrap();
        assert!(u.is_https());
        assert_eq!(
            u.uri("/json_rpc").unwrap().to_string(),
            "https://node.example.org/api/json_rpc"
        );

        let u = DaemonUrl::from_host_port("::1", 11898).unwrap();
        assert_eq!(u.authority(), "[::1]:11898");
        let u = DaemonUrl::from_host_port("localhost", 11899).unwrap();
        assert_eq!(u.to_string(), "http://localhost:11899");

        for bad in [
            "ftp://node:1",
            "http://",
            "http://user:pw@node:1",
            "http://node:1/?x=1",
            "",
        ] {
            assert!(DaemonUrl::parse(bad).is_err(), "{bad}");
        }
    }

    /// Building a transport needs neither a runtime nor, for `http://`, TLS settings.
    #[test]
    fn plain_transports_build_without_a_runtime() {
        let t = HttpTransport::new("http://127.0.0.1:1", &HttpConfig::default()).unwrap();
        assert!(!t.url().is_https());
        assert_eq!(t.url().authority(), "127.0.0.1:1");
    }
}
