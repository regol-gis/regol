//! A blocking facade over a multiplexed HTTP/3 connection. All sources share
//! the I/O runtime. Each caller waits only for its own request stream.
use super::{Range, RangeResponse, Validator, invalid};
use bytes::{Buf, Bytes};
use h3::{client::SendRequest, error::StreamError};
use once_cell::sync::OnceCell;
use quinn::{ClientConfig, Endpoint, crypto::rustls::QuicClientConfig};
use std::{
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, time::timeout};
use ureq::http::Uri;

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

fn runtime() -> io::Result<&'static Runtime> {
    static RUNTIME: OnceCell<Runtime> = OnceCell::new();
    RUNTIME.get_or_try_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("regol-http3")
            .enable_all()
            .build()
    })
}

// A caller can already be inside its own async runtime. Run the future on
// ours without nesting Runtime::block_on or blocking a QUIC driver thread.
fn wait<T: Send + 'static>(
    future: impl Future<Output = io::Result<T>> + Send + 'static,
) -> io::Result<T> {
    let (tx, rx) = mpsc::sync_channel(1);
    runtime()?.spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.recv().map_err(io::Error::other)?
}

fn client_config() -> io::Result<&'static ClientConfig> {
    static CONFIG: OnceCell<ClientConfig> = OnceCell::new();
    CONFIG.get_or_try_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        if roots.is_empty() {
            return Err(io::Error::other("no TLS trust anchors for HTTP/3"));
        }
        config_with_roots(roots)
    })
}

fn config_with_roots(roots: rustls::RootCertStore) -> io::Result<ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(io::Error::other)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let mut config = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(tls).map_err(io::Error::other)?,
    ));
    // Detect a vanished UDP path while the range GET still has time for TCP.
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(quinn::VarInt::from_u32(10_000).into()));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

#[derive(Clone)]
pub(super) struct Client {
    connection: quinn::Connection,
    sender: SendRequest<h3_quinn::OpenStreams, Bytes>,
}

impl Client {
    pub(super) fn connect(uri: &Uri) -> io::Result<Self> {
        Self::connect_with(uri, client_config()?.clone())
    }

    fn connect_with(uri: &Uri, config: ClientConfig) -> io::Result<Self> {
        let host = uri
            .host()
            .unwrap()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let port = uri.port_u16().unwrap_or(443);
        wait(async move {
            timeout(CONNECT_TIMEOUT, async {
                let mut addresses = tokio::net::lookup_host((host.as_str(), port))
                    .await?
                    .peekable();
                while let Some(address) = addresses.next() {
                    let bind = if address.is_ipv4() {
                        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
                    } else {
                        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
                    };
                    let Ok(endpoint) = Endpoint::client(SocketAddr::new(bind, 0)) else {
                        continue;
                    };
                    let Ok(connecting) = endpoint.connect_with(config.clone(), address, &host)
                    else {
                        continue;
                    };
                    // Leave room to try another address family on dual-stack origins.
                    let budget = if address.ip().is_loopback() {
                        Duration::from_millis(40)
                    } else if addresses.peek().is_some() {
                        CONNECT_TIMEOUT / 2
                    } else {
                        CONNECT_TIMEOUT
                    };
                    if let Ok(Ok(connection)) = timeout(budget, connecting).await {
                        let (mut driver, sender) = h3::client::builder()
                            .max_field_section_size(64 * 1024)
                            .build(h3_quinn::Connection::new(connection.clone()))
                            .await
                            .map_err(io::Error::other)?;
                        tokio::spawn(async move {
                            driver.wait_idle().await;
                        });
                        return Ok(Self { connection, sender });
                    }
                }
                Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "HTTP/3 unavailable",
                ))
            })
            .await
            .map_err(|_| io::ErrorKind::TimedOut)?
        })
    }

    pub(super) fn close(&self) {
        self.connection.close(0u32.into(), b"source closed");
    }

    pub(super) fn timed_out(&self) -> bool {
        matches!(
            self.connection.close_reason(),
            Some(quinn::ConnectionError::TimedOut)
        )
    }

    pub(super) fn same_connection(&self, other: &Self) -> bool {
        self.connection.stable_id() == other.connection.stable_id()
    }

    pub(super) fn get(
        &self,
        uri: Uri,
        range: Range,
        started: Instant,
        deadline: Instant,
    ) -> io::Result<RangeResponse> {
        let mut sender = self.sender.clone();
        wait(async move {
            tokio::time::timeout_at(deadline.into(), async {
                let mut stream = sender
                    .send_request(range.request(&uri)?)
                    .await
                    .map_err(stream_error)?;
                stream.finish().await.map_err(stream_error)?;
                let response = loop {
                    let response = stream.recv_response().await.map_err(stream_error)?;
                    if !response.status().is_informational() {
                        break response;
                    }
                };
                let headers_at = Instant::now();
                if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                    // Delegate redirects to the existing HTTPS redirect and proxy policy.
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "follow redirect with HTTP/1.1",
                    ));
                }
                let (total, size) = range.validate(response.status(), response.headers())?;
                let mut bytes = vec![0; size].into_boxed_slice();
                let mut unread = &mut bytes[..];
                while let Some(mut chunk) = stream.recv_data().await.map_err(stream_error)? {
                    if chunk.remaining() > unread.len() {
                        return Err(invalid("range response body exceeds Content-Range"));
                    }
                    let (dest, tail) = unread.split_at_mut(chunk.remaining());
                    chunk.copy_to_slice(dest);
                    unread = tail;
                }
                if !unread.is_empty() {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                Ok(RangeResponse {
                    uri,
                    validator: Validator::from_headers(response.headers()),
                    total,
                    bytes,
                    time_to_headers: headers_at - started,
                    body_time: headers_at.elapsed(),
                })
            })
            .await
            .map_err(|_| io::ErrorKind::TimedOut)?
        })
    }
}

fn stream_error(error: StreamError) -> io::Error {
    use h3::error::Code;
    let kind = match &error {
        StreamError::RemoteTerminate {
            code: Code::H3_VERSION_FALLBACK | Code::H3_REQUEST_REJECTED,
            ..
        } => io::ErrorKind::ConnectionAborted,
        StreamError::RemoteTerminate { .. }
        | StreamError::StreamError { .. }
        | StreamError::HeaderTooBig { .. } => io::ErrorKind::InvalidData,
        // Includes connection close, GOAWAY and failures in the underlying QUIC transport.
        _ => io::ErrorKind::ConnectionAborted,
    };
    io::Error::new(kind, error)
}

#[cfg(test)]
mod tests;
