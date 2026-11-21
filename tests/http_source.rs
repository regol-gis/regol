#![cfg(feature = "http")]

use regol::{
    Library, Query,
    source::{HttpSource, Source},
};
use std::{
    io::{self, BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    thread,
};

const GOL: &[u8] = include_bytes!("fixtures/basic.gol");
const PAGE: usize = 4096;

// Each connection serves one scripted request. Closing the connection also
// lets tests send intentionally truncated bodies without waiting for a timeout.
fn server(
    requests: usize,
    respond: impl Fn(usize, String, &mut TcpStream) + Send + 'static,
) -> io::Result<(String, thread::JoinHandle<()>)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let url = format!("http://{}/fixture.gol", listener.local_addr()?);
    let worker = thread::spawn(move || {
        for i in 0..requests {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).unwrap();
                assert_ne!(n, 0, "request ended before the headers");
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line.to_ascii_lowercase());
            }
            respond(i, request, &mut stream);
        }
    });
    Ok((url, worker))
}

fn reply(stream: &mut TcpStream, start: usize, etag: &str, body_len: usize) {
    let end = (start + PAGE).min(GOL.len());
    write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{}\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n", end-1, GOL.len(), end-start).unwrap();
    stream
        .write_all(&GOL[start..(start + body_len).min(end)])
        .unwrap();
}

#[test]
fn concurrent_range_reads_share_validated_pages_and_query_matches_disk()
-> Result<(), Box<dyn std::error::Error>> {
    let pages = GOL.len().div_ceil(PAGE);
    let (url, worker) = server(pages, |i, request, stream| {
        assert!(request.starts_with("get /fixture.gol "));
        assert!(request.contains("accept-encoding: identity\r\n"));
        let start = i * PAGE;
        let end = if i == 0 {
            PAGE - 1
        } else {
            (start + PAGE).min(GOL.len()) - 1
        };
        assert!(request.contains(&format!("range: bytes={start}-{end}\r\n")));
        assert_eq!(request.contains("if-match: \"v1\"\r\n"), i != 0);
        reply(stream, start, "\"v1\"", PAGE);
    })?;
    let source = HttpSource::with_page_size(&url, PAGE)?;
    assert_eq!(source.len(), GOL.len() as u64);
    assert!(source.retained_tail(PAGE as u64).is_none());
    assert!(source.view(PAGE as u64 - 1, 2)?.is_none());
    assert_eq!(source.view(source.len(), 0)?, Some(&[][..]));
    assert!(source.read_exact_at(u64::MAX, &mut []).is_err());
    assert!(source.segment(source.len()).is_err());
    thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                let mut span = [0; 64];
                source.read_exact_at(PAGE as u64 - 8, &mut span).unwrap();
                assert_eq!(span, GOL[PAGE - 8..PAGE + 56]);
            });
        }
    });
    assert_eq!(source.cached_pages(), 2);
    let mut full = vec![0; GOL.len()];
    source.read_exact_at(0, &mut full)?;
    assert_eq!(full, GOL);
    assert_eq!(source.cached_pages(), pages as u64);
    assert_eq!(source.cached_bytes(), GOL.len() as u64);
    worker.join().unwrap();
    let lib = Library::new(source)?;
    let disk = Library::open("tests/fixtures/basic.gol")?;
    let q = Query::parse("na[amenity=cafe]")?;
    let expected = q
        .prepare(&disk)?
        .iter()
        .map(|f| f.map(|f| f.id()))
        .collect::<regol::Result<Vec<_>>>()?;
    let actual = q
        .prepare(&lib)?
        .iter()
        .map(|f| f.map(|f| f.id()))
        .collect::<regol::Result<Vec<_>>>()?;
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn failed_or_changed_pages_are_never_installed() -> io::Result<()> {
    let (url, worker) = server(6, |i, _, stream| match i {
        0 => reply(stream, 0, "\"v1\"", PAGE),
        1 => reply(stream, PAGE, "\"v1\"", 7),
        2 => reply(stream, PAGE, "\"v2\"", PAGE),
        3 => reply(stream, 0, "\"v1\"", PAGE),
        4 => write!(
            stream,
            "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap(),
        _ => reply(stream, PAGE, "\"v1\"", PAGE),
    })?;
    let source = HttpSource::with_page_size(&url, PAGE)?;
    for _ in 0..4 {
        assert!(source.segment(PAGE as u64).is_err());
        assert!(source.retained_tail(PAGE as u64).is_none());
        assert_eq!(source.cached_pages(), 1);
    }
    assert_eq!(source.view(PAGE as u64, 16)?, Some(&GOL[PAGE..PAGE + 16]));
    worker.join().unwrap();
    Ok(())
}

#[test]
fn refuses_ignored_encoded_or_inconsistent_ranges() -> io::Result<()> {
    let replies = [
        "200 OK\r\nContent-Length: 9000000000000",
        "206 Partial Content\r\nContent-Length: 1",
        "206 Partial Content\r\nContent-Range: bytes 0-0/*",
        "206 Partial Content\r\nContent-Range: bytes 1-1/2",
        "206 Partial Content\r\nContent-Range: bytes 0-4/4",
        "206 Partial Content\r\nContent-Range: bytes 0-0/+1",
        "206 Partial Content\r\nContent-Range: bytes 0-0/1\r\nContent-Range: bytes 1-1/2",
        "206 Partial Content\r\nContent-Range: bytes 0-0/1\r\nContent-Encoding: gzip",
        "206 Partial Content\r\nContent-Range: bytes 0-0/1\r\nContent-Length: 8",
    ];
    let (url, worker) = server(replies.len(), move |i, _, stream| {
        write!(
            stream,
            "HTTP/1.1 {}\r\nConnection: close\r\n\r\nX",
            replies[i]
        )
        .unwrap();
    })?;
    for _ in replies {
        assert!(HttpSource::open(&url).is_err());
    }
    worker.join().unwrap();
    Ok(())
}

#[test]
fn weak_etag_uses_modification_date_and_accepts_exact_chunked_bodies() -> io::Result<()> {
    const DATE: &str = "Thu, 19 Nov 2026 12:00:00 GMT";
    let (url, worker) = server(3, |i, request, stream| {
        let date = format!("if-unmodified-since: {}\r\n", DATE.to_ascii_lowercase());
        assert_eq!(request.contains(&date), i != 0);
        assert!(!request.contains("if-match:"));
        let (first, last, size) = match i {
            0 => (0, PAGE - 1, PAGE),
            // A chunk containing an extra byte must fail.
            1 => (PAGE, PAGE, 2),
            _ => (PAGE, PAGE, 1),
        };
        write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {first}-{last}/{}\r\nETag: W/\"v1\"\r\nLast-Modified: {DATE}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{size:x}\r\n", PAGE+1).unwrap();
        stream.write_all(&vec![17; size]).unwrap();
        stream.write_all(b"\r\n0\r\n\r\n").unwrap();
    })?;
    let source = HttpSource::with_page_size(&url, PAGE)?;
    assert!(source.segment(PAGE as u64).is_err());
    assert_eq!(source.view(PAGE as u64, 1)?, Some(&[17][..]));
    worker.join().unwrap();
    Ok(())
}

#[cfg(feature = "parallel")]
#[test]
fn bounded_http_workers_preserve_tile_deduplication_and_traversal_limit()
-> Result<(), Box<dyn std::error::Error>> {
    use regol::{Bounds, Limits, QueryExecutor};
    let bytes = include_bytes!("fixtures/tiled_global.gol");
    let (url, worker) = server(2 * bytes.len().div_ceil(PAGE), move |_, headers, stream| {
        let range = headers
            .lines()
            .find_map(|line| line.strip_prefix("range: bytes="))
            .unwrap();
        let (start, end) = range.split_once('-').unwrap();
        let start = start.parse::<usize>().unwrap();
        let end = end.parse::<usize>().unwrap();
        assert_eq!(end, (start + PAGE).min(bytes.len()) - 1);
        write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len(), end-start+1).unwrap();
        stream.write_all(&bytes[start..=end]).unwrap();
    })?;
    let pool = QueryExecutor::new(4)?;
    let query = Query::parse("*")?;
    let window = Bounds::new(i32::MIN + 1, i32::MIN + 1, i32::MAX, i32::MAX)?;
    // Exercise worker handoff with exactly the steps needed, then one fewer.
    let (mut low, mut high) = (0, 1_000_000);
    while low < high {
        let mid = low + (high - low) / 2;
        let disk = Library::with_limits(
            bytes.as_slice(),
            Limits {
                max_traversal_steps: mid,
                ..Limits::default()
            },
        )?;
        if query.prepare(&disk)?.count_in_bounds(window).is_ok() {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    for limit in [low, low - 1] {
        let limits = Limits {
            max_traversal_steps: limit,
            ..Limits::default()
        };
        let remote = Library::with_limits(HttpSource::with_page_size(&url, PAGE)?, limits)?;
        let local = Library::with_limits(bytes.as_slice(), limits)?;
        let prepared = query.prepare(&remote)?;
        let disk = query.prepare(&local)?;
        assert!(remote.source().prefers_concurrent_tile_reads());
        let before = remote.source().cached_pages();
        pool.with_query(&prepared, Some(window), |_| ())?;
        assert_eq!(remote.source().cached_pages(), before);
        if limit == low {
            let caller = thread::current().id();
            let matches_on_caller = |executor: &QueryExecutor| {
                executor.try_fold_reduce(
                    &prepared,
                    Some(window),
                    || 0u64,
                    |n, _| Ok(n + u64::from(thread::current().id() == caller)),
                    |a, b| Ok(a + b),
                )
            };
            let one = QueryExecutor::new(1)?;
            let first = one.with_query(&prepared, Some(window), |items| {
                items.next().transpose().map(|f| f.map(|f| f.id()))
            })??;
            assert_eq!(
                first,
                disk.in_bounds(window).next().transpose()?.map(|f| f.id())
            );
            assert_eq!(matches_on_caller(&pool)?, 0);
            let serial_prefix = QueryExecutor::new(4)?.with_reduction_prefix_steps(u64::MAX);
            assert_eq!(
                matches_on_caller(&serial_prefix)?,
                disk.count_in_bounds(window)?
            );
            pool.with_query(&prepared, Some(window), |items| items.next().unwrap())??;
        }
        for bounds in [window, Bounds::new(-1, -1, 1, 1)?, Bounds::WORLD] {
            let expected = disk.count_in_bounds(bounds);
            let mut ids = pool.with_query(&prepared, Some(bounds), |items| {
                items
                    .map(|f| f.map(|f| f.id()))
                    .collect::<regol::Result<Vec<_>>>()
            })?;
            if let Ok(actual) = &mut ids {
                let mut expected_ids = disk
                    .in_bounds(bounds)
                    .map(|f| f.map(|f| f.id()))
                    .collect::<regol::Result<Vec<_>>>()?;
                actual.sort_unstable();
                expected_ids.sort_unstable();
                assert_eq!(*actual, expected_ids);
            }
            for count in [
                pool.count(&prepared, Some(bounds)),
                QueryExecutor::count_once(4, &prepared, Some(bounds)),
                ids.map(|ids| ids.len() as u64),
                pool.try_fold_reduce(
                    &prepared,
                    Some(bounds),
                    || 0,
                    |n, _| Ok(n + 1),
                    |a, b| Ok(a + b),
                ),
            ] {
                match (&expected, count) {
                    (Ok(n), Ok(actual)) => assert_eq!(*n, actual),
                    (Err(regol::Error::Limit(_)), Err(regol::Error::Limit(_))) => (),
                    (expected, actual) => panic!("disk {expected:?}, HTTP {actual:?}"),
                }
            }
        }
        remote
            .source()
            .read_exact_at(0, &mut vec![0; bytes.len()])?;
        assert!(!remote.source().prefers_concurrent_tile_reads());
    }
    worker.join().unwrap();
    Ok(())
}

#[test]
fn cancellation_interrupts_stalled_headers_and_bodies() -> io::Result<()> {
    use std::{io::Read, sync::mpsc, time::Duration};
    for send_headers in [false, true] {
        let (started, pending) = mpsc::channel();
        let (url, server) = server(2, move |i, _, stream| {
            if i == 0 {
                return reply(stream, 0, "\"v1\"", PAGE);
            }
            if send_headers {
                write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4096-8191/{}\r\nContent-Length: 4096\r\nETag: \"v1\"\r\n\r\nx", GOL.len()).unwrap();
            }
            started.send(()).unwrap();
            // The server holds the response open until the client closes the socket.
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let result = stream.read(&mut [0]);
            assert!(
                matches!(result, Ok(0))
                    || result.is_err_and(|e| e.kind() == io::ErrorKind::ConnectionReset)
            );
        })?;
        let source = HttpSource::with_page_size(&url, PAGE)?;
        thread::scope(|scope| {
            let loading = scope.spawn(|| source.view(PAGE as u64, 1));
            pending.recv_timeout(Duration::from_secs(3)).unwrap();
            source.cancel_requests();
            assert!(loading.join().unwrap().is_err());
        });
        server.join().unwrap();
        assert!(source.segment(PAGE as u64).is_err());
        assert_eq!(source.cached_pages(), 1);
        assert_eq!(source.view(0, 4)?, Some(&GOL[..4]));
    }
    Ok(())
}

#[test]
fn successive_range_gets_reuse_a_keep_alive_connection() -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let url = format!("http://{}/gol", listener.local_addr()?);
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let mut socket = BufReader::new(stream);
        for first in [0, PAGE, 2 * PAGE] {
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(socket.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line.to_ascii_lowercase());
            }
            let last = first + PAGE - 1;
            assert!(headers.contains(&format!("range: bytes={first}-{last}\r\n")));
            write!(socket.get_mut(),"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {first}-{last}/{}\r\nContent-Length: {PAGE}\r\n\r\n",GOL.len()).unwrap();
            socket.get_mut().write_all(&GOL[first..=last]).unwrap();
        }
    });
    let source = HttpSource::with_page_size(&url, PAGE)?;
    assert_eq!(source.view(PAGE as u64, 1)?, Some(&GOL[PAGE..PAGE + 1]));
    assert_eq!(
        source.view(2 * PAGE as u64, 1)?,
        Some(&GOL[2 * PAGE..2 * PAGE + 1])
    );
    server.join().unwrap();
    Ok(())
}
