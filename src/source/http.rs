mod quic;

use super::{PageDirectory, Source};
use once_cell::sync::OnceCell;
use std::{
    io::{self, Read},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use ureq::{
    Agent, ResponseExt,
    http::{HeaderMap, HeaderValue, Request, StatusCode, Uri, header},
    tls::{RootCerts, TlsConfig, TlsProvider},
};

/// Random access to an immutable HTTP(S) GOL. Direct HTTPS prefers multiplexed
/// HTTP/3 on the origin port and falls back to pooled HTTP/1.1.
/// The server must return single identity-encoded byte ranges with a known total
/// length. Each touched page is shared across readers and retained until drop.
/// A full scan can therefore retain the entire object. A strong ETag pins each
/// subsequent GET. Last-Modified is a weaker fallback with one-second precision.
/// Use a versioned URL when the origin cannot guarantee stable contents.
pub struct HttpSource {
    transport: Transport,
    uri: Uri,
    len: u64,
    shift: u32,
    first: Box<[u8]>,
    validator: Option<Validator>,
    pages: PageDirectory<OnceCell<Box<[u8]>>>,
    loaded_pages: AtomicU64,
    loaded_bytes: AtomicU64,
}

#[derive(Clone)]
enum Validator {
    Etag(HeaderValue),
    Modified(HeaderValue),
}

impl Validator {
    fn from_headers(headers: &HeaderMap) -> Option<Self> {
        if let Some(tag) = headers.get(header::ETAG) {
            let bytes = tag.as_bytes();
            if bytes.len() >= 2 && bytes.starts_with(b"\"") && bytes.ends_with(b"\"") {
                return Some(Self::Etag(tag.clone()));
            }
        }
        headers
            .get(header::LAST_MODIFIED)
            .cloned()
            .map(Self::Modified)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Clone)]
struct Range {
    start: u64,
    end: u64,
    expected_total: Option<u64>,
    validator: Option<Validator>,
}

struct RangeResponse {
    uri: Uri,
    validator: Option<Validator>,
    total: u64,
    bytes: Box<[u8]>,
    time_to_headers: Duration,
    body_time: Duration,
}

impl Range {
    fn request(&self, uri: &Uri) -> io::Result<Request<()>> {
        let mut request = Request::get(uri.clone())
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::RANGE, format!("bytes={}-{}", self.start, self.end));
        if let Some(validator) = &self.validator {
            let (condition, value) = match validator {
                Validator::Etag(tag) => (header::IF_MATCH, tag),
                Validator::Modified(date) => (header::IF_UNMODIFIED_SINCE, date),
            };
            request = request.header(condition, value);
        }
        request.body(()).map_err(io::Error::other)
    }

    fn validate(&self, status: StatusCode, headers: &HeaderMap) -> io::Result<(u64, usize)> {
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(io::Error::other(format!(
                "range GET bytes={}-{} returned HTTP {status} (expected 206)",
                self.start, self.end
            )));
        }
        if headers
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(invalid("range response has unexpected Content-Encoding"));
        }
        if let Some(validator) = &self.validator {
            let (name, expected) = match validator {
                Validator::Etag(tag) => (header::ETAG, tag),
                Validator::Modified(date) => (header::LAST_MODIFIED, date),
            };
            if headers.get(name) != Some(expected) {
                return Err(invalid(
                    "range response object validator changed or disappeared",
                ));
            }
        }
        let mut fields = headers.get_all(header::CONTENT_RANGE).iter();
        let range = fields
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(parse_range);
        let (first, last, total) = range
            .filter(|_| fields.next().is_none())
            .ok_or_else(|| invalid("missing or invalid Content-Range"))?;
        if self.start >= total
            || first != self.start
            || last != self.end.min(total - 1)
            || self.expected_total.is_some_and(|n| n != total)
        {
            return Err(invalid(
                "Content-Range does not match the requested range or object length",
            ));
        }
        let size = (last - first + 1) as usize;
        if let Some(length) = headers.get(header::CONTENT_LENGTH) {
            if length.to_str().ok().and_then(|s| s.parse::<usize>().ok()) != Some(size) {
                return Err(invalid("Content-Length does not match Content-Range"));
            }
        }
        Ok((total, size))
    }
}

fn parse_range(field: &str) -> Option<(u64, u64, u64)> {
    fn decimal(s: &str) -> Option<u64> {
        if !s.as_bytes().iter().all(u8::is_ascii_digit) {
            return None;
        }
        s.parse().ok()
    }
    let (unit, range) = field.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (span, total) = range.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    Some((decimal(first)?, decimal(last)?, decimal(total)?))
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct Transport {
    agent: Agent,
    quic: Mutex<Option<quic::Client>>,
    cancelled: AtomicBool,
}

impl Transport {
    fn new(uri: &Uri) -> Self {
        let agent = Agent::config_builder()
            .https_only(uri.scheme_str() == Some("https"))
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::NativeTls)
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .new_agent();
        let direct = agent
            .config()
            .proxy()
            .is_none_or(|proxy| proxy.is_no_proxy(uri));
        let quic = if uri.scheme_str() == Some("https")
            && direct
            && uri.authority().is_some_and(|a| !a.as_str().contains('@'))
        {
            quic::Client::connect(uri).ok()
        } else {
            None
        };
        Self {
            agent,
            quic: Mutex::new(quic),
            cancelled: AtomicBool::new(false),
        }
    }

    fn check_cancelled(&self) -> io::Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "HTTP source cancelled",
            ));
        }
        Ok(())
    }

    fn get(&self, uri: &Uri, range: Range, started: Instant) -> io::Result<RangeResponse> {
        self.check_cancelled()?;
        let deadline = started + REQUEST_TIMEOUT;
        let quic = {
            let mut slot = self.quic.lock().unwrap();
            // An otherwise healthy source can sit idle between queries. Reconnect
            // once for all callers if QUIC already expired before this GET.
            if slot.as_ref().is_some_and(quic::Client::timed_out) {
                self.check_cancelled()?;
                *slot = quic::Client::connect(uri).ok();
            }
            slot.clone()
        };
        if let Some(client) = quic {
            match client.get(uri.clone(), range.clone(), started, deadline) {
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                    // Retry this idempotent GET with its original validator and deadline.
                    self.check_cancelled()?;
                    let mut slot = self.quic.lock().unwrap();
                    if slot
                        .as_ref()
                        .is_some_and(|current| current.same_connection(&client))
                    {
                        *slot = None;
                        client.close();
                    }
                }
                result => return result,
            }
        }
        self.check_cancelled()?;
        let started = Instant::now();
        let remaining = deadline
            .checked_duration_since(started)
            .ok_or(io::ErrorKind::TimedOut)?;
        let request = self
            .agent
            .configure_request(range.request(uri)?)
            .timeout_global(Some(remaining))
            .build();
        let mut response = self.agent.run(request).map_err(ureq::Error::into_io)?;
        let headers_at = Instant::now();
        let (total, size) = range.validate(response.status(), response.headers())?;
        let mut bytes = vec![0; size].into_boxed_slice();
        let mut body = response.body_mut().as_reader();
        body.read_exact(&mut bytes)?;
        if body.read(&mut [0])? != 0 {
            return Err(invalid("range response body exceeds Content-Range"));
        }
        Ok(RangeResponse {
            uri: response.get_uri().clone(),
            validator: Validator::from_headers(response.headers()),
            total,
            bytes,
            time_to_headers: headers_at - started,
            body_time: headers_at.elapsed(),
        })
    }
}

const PROBE_BYTES: usize = 128 * 1024;

// Time to headers includes DNS and connection establishment on a new origin.
// Avoid using a sub-millisecond sample to inflate loopback transfers. When the
// payload dominates, smaller pages reduce bytes spent on sparse reads.
fn adaptive_page_size(headers: Duration, body: Duration) -> usize {
    if headers >= Duration::from_millis(5) && headers / 4 >= body {
        4 * PROBE_BYTES
    } else if body >= Duration::from_millis(5) && body / 4 >= headers {
        PROBE_BYTES / 4
    } else {
        PROBE_BYTES
    }
}

impl HttpSource {
    /// Open with a 128 KiB range. Select subsequent page sizes from the
    /// observed time to headers and time to read that first payload.
    ///
    /// ```no_run
    /// use regol::{Bounds, DictionaryLoading, Library, Limits, Query, source::HttpSource};
    ///
    /// fn count_roads(url: &str, bounds: Bounds) -> regol::Result<u64> {
    ///     let source = HttpSource::open(url)?;
    ///     let library = Library::with_dictionary_loading(
    ///         source, Limits::default(), DictionaryLoading::OnDemand,
    ///     )?;
    ///     Query::parse("w[highway]")?.prepare(&library)?.count_in_bounds(bounds)
    /// }
    /// ```
    pub fn open(url: &str) -> io::Result<Self> {
        Self::connect(url, None)
    }

    /// Open with a fixed aligned power-of-two range size between 4 KiB and 16 MiB.
    pub fn with_page_size(url: &str, page_size: usize) -> io::Result<Self> {
        Self::connect(url, Some(page_size))
    }

    fn connect(url: &str, fixed_size: Option<usize>) -> io::Result<Self> {
        let page_size = fixed_size.unwrap_or(PROBE_BYTES);
        if !page_size.is_power_of_two() || !(4096..=16 << 20).contains(&page_size) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HTTP page size must be a power of two from 4 KiB to 16 MiB",
            ));
        }
        let uri: Uri = url
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected an absolute HTTP(S) URL",
            ));
        }
        let started = Instant::now();
        let transport = Transport::new(&uri);
        let opening = Range {
            start: 0,
            end: page_size as u64 - 1,
            expected_total: None,
            validator: None,
        };
        let response = transport.get(&uri, opening, started)?;
        Self::from_opening(transport, response, fixed_size)
    }

    fn from_opening(
        transport: Transport,
        response: RangeResponse,
        fixed_size: Option<usize>,
    ) -> io::Result<Self> {
        let len = response.total;
        let first = response.bytes;
        let page_size = fixed_size
            .unwrap_or_else(|| adaptive_page_size(response.time_to_headers, response.body_time));
        // GOL 2 uses 32-bit page numbers in units of 4096 bytes. Bound the
        // sparse directory before allocating from an untrusted object length.
        if len > (1u64 << 44) {
            return Err(invalid("object too large for GOL 2"));
        }
        let count = usize::try_from(len.div_ceil(page_size as u64))
            .map_err(|_| invalid("too many HTTP pages"))?;
        let pages: PageDirectory<OnceCell<Box<[u8]>>> = PageDirectory::sparse(count)?;
        let loaded_bytes = AtomicU64::new(first.len() as u64);
        Ok(Self {
            transport,
            uri: response.uri,
            len,
            shift: page_size.trailing_zeros(),
            first,
            validator: response.validator,
            pages,
            loaded_pages: AtomicU64::new(1),
            loaded_bytes,
        })
    }

    /// Size of subsequent aligned reads, in bytes. The opening range and ranges
    /// adjacent to the opening range or EOF can have different lengths.
    pub fn page_bytes(&self) -> usize {
        1 << self.shift
    }

    /// Permanently cancel network I/O for this source. Call from another thread
    /// to interrupt blocked socket reads, including response bodies. All queries
    /// sharing the source are affected. Cached pages remain readable. DNS and TCP
    /// connection establishment are subject to the 30-second request timeout.
    pub fn cancel_requests(&self) {
        self.transport.cancelled.store(true, Ordering::Release);
        self.transport.agent.clone().shutdown();
        if let Some(client) = self.transport.quic.lock().unwrap().as_ref() {
            client.close();
        }
    }

    /// Protocol selected for network reads on this source, either `HTTP/3` or `HTTP/1.1`.
    pub fn protocol(&self) -> &'static str {
        if self.transport.quic.lock().unwrap().is_some() {
            "HTTP/3"
        } else {
            "HTTP/1.1"
        }
    }

    /// Number of successful range fills, including the opening GET.
    pub fn cached_pages(&self) -> u64 {
        self.loaded_pages.load(Ordering::Relaxed)
    }
    /// Total payload bytes retained from successful range GETs.
    pub fn cached_bytes(&self) -> u64 {
        self.loaded_bytes.load(Ordering::Relaxed)
    }

    fn page_start(&self, offset: u64) -> u64 {
        if offset < self.first.len() as u64 {
            0
        } else {
            ((offset >> self.shift) << self.shift).max(self.first.len() as u64)
        }
    }

    fn tail(&self, offset: u64) -> io::Result<&[u8]> {
        let start = self.page_start(offset);
        let bytes = self.page(offset)?;
        Ok(&bytes[(offset - start) as usize..])
    }

    fn page(&self, offset: u64) -> io::Result<&[u8]> {
        if offset >= self.len {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if offset < self.first.len() as u64 {
            return Ok(&self.first);
        }
        let index = (offset >> self.shift) as usize;
        let slot = self
            .pages
            .slot::<true>(index)
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        let page = slot.get_or_try_init(|| -> io::Result<_> {
            let start = self.page_start(offset);
            // Clip page zero to the end of the opening probe if it was smaller.
            let end = ((index as u64 + 1) << self.shift).min(self.len) - 1;
            let range = Range {
                start,
                end,
                expected_total: Some(self.len),
                validator: self.validator.clone(),
            };
            let bytes = self.transport.get(&self.uri, range, Instant::now())?.bytes;
            self.loaded_bytes
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            self.loaded_pages.fetch_add(1, Ordering::Relaxed);
            Ok(bytes)
        })?;
        Ok(page)
    }

    fn check(&self, offset: u64, size: usize) -> io::Result<()> {
        if offset
            .checked_add(size as u64)
            .is_none_or(|end| end > self.len)
        {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }
}

impl Source for HttpSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn prefers_concurrent_tile_reads(&self) -> bool {
        self.cached_bytes() < self.len
    }

    fn retained_tail(&self, offset: u64) -> Option<&[u8]> {
        if offset >= self.len {
            return None;
        }
        if offset < self.first.len() as u64 {
            return self.first.get(offset as usize..);
        }
        let slot = self.pages.slot::<false>((offset >> self.shift) as usize)?;
        slot.get()?
            .get((offset - self.page_start(offset)) as usize..)
    }

    fn segment(&self, offset: u64) -> io::Result<Option<(u64, &[u8])>> {
        Ok(Some((self.page_start(offset), self.page(offset)?)))
    }

    fn view(&self, offset: u64, len: usize) -> io::Result<Option<&[u8]>> {
        self.check(offset, len)?;
        if len == 0 {
            return Ok(Some(&[]));
        }
        if self.page_start(offset) != self.page_start(offset + len as u64 - 1) {
            return Ok(None);
        }
        Ok(Some(&self.tail(offset)?[..len]))
    }

    fn read_exact_at(&self, mut offset: u64, mut output: &mut [u8]) -> io::Result<()> {
        self.check(offset, output.len())?;
        while !output.is_empty() {
            let tail = self.tail(offset)?;
            let count = tail.len().min(output.len());
            output[..count].copy_from_slice(&tail[..count]);
            output = &mut output[count..];
            offset += count as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_sample_accounts_for_both_request_latency_and_bandwidth() {
        for (headers_ms, body_ms, kib) in [(0, 0, 128), (100, 5, 512), (2, 34, 32), (12, 10, 128)] {
            assert_eq!(
                adaptive_page_size(
                    Duration::from_millis(headers_ms),
                    Duration::from_millis(body_ms)
                ),
                kib * 1024
            );
        }
    }
}
