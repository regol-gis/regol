use super::*;
use crate::source::{HttpSource, Source, http::Transport};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    thread,
};
use ureq::{
    Agent,
    http::{Response, header},
    tls::{Certificate, RootCerts, TlsConfig, TlsProvider},
};

const PAGE: usize = 4096;
const TOTAL: usize = 4 * PAGE;
type ServerStream = h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

// Accept exactly one QUIC connection. The handler controls the order in which
// overlapping streams receive headers and data.
fn origin<F>(
    serve: impl FnOnce(h3::server::Connection<h3_quinn::Connection, Bytes>, quinn::Connection) -> F
    + Send
    + 'static,
) -> (Uri, Transport, TcpListener, rustls::ServerConfig)
where
    F: Future<Output = ()> + Send + 'static,
{
    // Public test identity for the loopback origin. Only these clients trust it.
    let der = CertificateDer::from(include_bytes!("localhost-cert.der").as_slice());
    let key = PrivatePkcs8KeyDer::from(include_bytes!("localhost-key.der").as_slice());
    let tls = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![der.clone()], key.into())
    .unwrap();
    let mut h3_tls = tls.clone();
    h3_tls.alpn_protocols = vec![b"h3".to_vec()];
    let config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(h3_tls).unwrap(),
    ));
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = tcp.local_addr().unwrap();
    let _guard = runtime().unwrap().enter();
    let endpoint = Endpoint::server(config, addr).unwrap();
    runtime().unwrap().spawn(async move {
        let connection = endpoint.accept().await.unwrap().await.unwrap();
        let server = h3::server::Connection::new(h3_quinn::Connection::new(connection.clone()))
            .await
            .unwrap();
        serve(server, connection).await;
    });
    let uri: Uri = format!("https://{addr}/test.gol").parse().unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der.clone()).unwrap();
    let quic = Client::connect_with(&uri, config_with_roots(roots).unwrap()).unwrap();
    let agent = Agent::config_builder()
        .proxy(None)
        .http_status_as_error(false)
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::new_with_certs(&[
                    Certificate::from_der(&der).to_owned()
                ]))
                .build(),
        )
        .build()
        .new_agent();
    (
        uri,
        Transport {
            agent,
            quic: Some(quic).into(),
            cancelled: false.into(),
        },
        tcp,
        tls,
    )
}

async fn receive(
    server: &mut h3::server::Connection<h3_quinn::Connection, Bytes>,
) -> (usize, ServerStream) {
    let (req, stream) = server
        .accept()
        .await
        .unwrap()
        .unwrap()
        .resolve_request()
        .await
        .unwrap();
    let range = req.headers()[header::RANGE]
        .to_str()
        .unwrap()
        .strip_prefix("bytes=")
        .unwrap();
    let (start, end) = range.split_once('-').unwrap();
    let start = start.parse::<usize>().unwrap();
    assert_eq!(end.parse::<usize>().unwrap(), start + PAGE - 1);
    assert_eq!(
        req.headers().get(header::IF_MATCH).map(|v| v.as_bytes()),
        (start > 0).then_some(&b"\"v1\""[..])
    );
    (start, stream)
}

async fn headers(stream: &mut ServerStream, start: usize) {
    stream
        .send_response(
            Response::builder()
                .status(206)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{}/{TOTAL}", start + PAGE - 1),
                )
                .header(header::ETAG, "\"v1\"")
                .body(())
                .unwrap(),
        )
        .await
        .unwrap();
}

async fn opening(server: &mut h3::server::Connection<h3_quinn::Connection, Bytes>) {
    let (start, mut stream) = receive(server).await;
    assert_eq!(start, 0);
    headers(&mut stream, start).await;
    stream.send_data(Bytes::from(vec![1; PAGE])).await.unwrap();
    stream.finish().await.unwrap();
}

fn open(uri: Uri, transport: Transport) -> HttpSource {
    let first = transport
        .get(
            &uri,
            Range {
                start: 0,
                end: PAGE as u64 - 1,
                expected_total: None,
                validator: None,
            },
            Instant::now(),
        )
        .unwrap();
    HttpSource::from_opening(transport, first, Some(PAGE)).unwrap()
}

#[test]
fn cancellation_wakes_multiplexed_streams_and_preserves_cached_bytes() {
    let (tx, rx) = mpsc::channel();
    let (uri, transport, tcp, _) = origin(move |mut server, conn| async move {
        opening(&mut server).await;
        let (_, _waiting_for_headers) = receive(&mut server).await;
        let (start, mut partial) = receive(&mut server).await;
        headers(&mut partial, start).await;
        partial.send_data(Bytes::from_static(&[2])).await.unwrap();
        tx.send(()).unwrap();
        conn.closed().await;
    });
    let source = open(uri, transport);
    assert_eq!(source.protocol(), "HTTP/3");
    thread::scope(|scope| {
        let first = scope.spawn(|| source.segment(PAGE as u64));
        let second = scope.spawn(|| source.segment(2 * PAGE as u64));
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        source.cancel_requests();
        assert!(first.join().unwrap().is_err());
        assert!(second.join().unwrap().is_err());
    });
    assert_eq!(source.cached_pages(), 1);
    assert_eq!(source.view(0, 1).unwrap(), Some(&[1][..]));
    assert!(source.segment(3 * PAGE as u64).is_err());
    tcp.set_nonblocking(true).unwrap();
    assert_eq!(tcp.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn lost_quic_range_retries_on_one_pooled_tls_connection_with_original_validator() {
    let (uri, transport, tcp, tls) = origin(|mut server, conn| async move {
        opening(&mut server).await;
        let (_, mut changed) = receive(&mut server).await;
        changed
            .send_response(Response::builder().status(412).body(()).unwrap())
            .await
            .unwrap();
        changed.finish().await.unwrap();
        let (start, mut stream) = receive(&mut server).await;
        headers(&mut stream, start).await;
        stream.send_data(Bytes::from_static(&[99])).await.unwrap();
        conn.close(0u32.into(), b"test disconnect");
    });
    let source = open(uri, transport);
    assert!(source.segment(PAGE as u64).is_err());
    assert_eq!(source.cached_pages(), 1);
    assert_eq!(source.protocol(), "HTTP/3");
    let server = thread::spawn(move || {
        let conn = rustls::ServerConnection::new(tls.into()).unwrap();
        let sock = tcp.accept().unwrap().0;
        sock.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut reader = BufReader::new(rustls::StreamOwned::new(conn, sock));
        for start in [PAGE, 2 * PAGE] {
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line.to_ascii_lowercase());
            }
            assert!(request.starts_with("get /test.gol http/1.1\r\n"));
            assert!(request.contains("if-match: \"v1\"\r\n"));
            let end = start + PAGE - 1;
            assert!(request.contains(&format!("range: bytes={start}-{end}\r\n")));
            let stream = reader.get_mut();
            write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{TOTAL}\r\nETag: \"v1\"\r\nContent-Length: {PAGE}\r\n\r\n").unwrap();
            stream.write_all(&[2; PAGE]).unwrap();
            stream.flush().unwrap();
        }
    });
    for start in [PAGE, 2 * PAGE] {
        assert_eq!(
            source.view(start as u64, PAGE).unwrap().unwrap(),
            &[2; PAGE]
        );
        assert_eq!(source.protocol(), "HTTP/1.1");
    }
    server.join().unwrap();
}
