//! Storage backends for [`crate::Library`]. Choose storage when opening a file.
//!
//! - [`PagedFile`] retains touched pages for repeated reads. [`crate::Library::open`] uses it.
//! - [`CachedFile`] shares a bounded page cache across readers.
//! - [`WindowedFile`] gives each active iterator a small reusable read buffer.
//! - [`FileSource`] issues positioned reads directly.
//! - With the `http` feature, `HttpSource` retains ranged reads from an HTTP(S) URL.
//! - A byte slice or `Vec<u8>` uses memory the application already holds.
//!
//! A consistent query requires stable file contents. The optional `mmap` feature
//! also provides `MappedFile`, whose safety contract requires an immutable backing file.
use std::{fs::File, io, path::Path, sync::OnceLock};

/// Random access to a finite byte source. Implementations must fill the whole
/// output or return an error. A short read must never be reported as success.
pub trait Source {
    /// Length of the source in bytes.
    fn len(&self) -> u64;
    /// Whether the source contains zero bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Read the entire buffer starting at an absolute byte offset or return an error.
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()>;
    /// Optional size hint for reusable, iterator-owned read buffers when no
    /// stable segment is available. Zero disables buffering. Readers cap this
    /// hint at 64 KiB; returned feature handles never borrow these buffers.
    #[inline]
    fn read_buffer_size(&self) -> usize {
        0
    }
    /// Whether parallel scans should give whole tiles to demand workers so their
    /// payload reads can overlap. Intended for high-latency random access.
    fn prefers_concurrent_tile_reads(&self) -> bool {
        false
    }
    /// Notify an incremental source that a traversal needs a missing tile.
    #[cfg(feature = "gob")]
    #[doc(hidden)]
    fn missing_tile(&self, _tip: u32) {}
    /// Optional zero-copy access; the returned slice must cover the whole source.
    #[inline]
    fn as_bytes(&self) -> Option<&[u8]> {
        None
    }
    /// Borrow a checked range when this backend can keep it stable. Backends
    /// may return `None` (for example, for a range spanning cached pages).
    #[inline]
    fn view(&self, offset: u64, len: usize) -> io::Result<Option<&[u8]>> {
        self.as_bytes()
            .map(|bytes| checked_range(bytes, offset, len))
            .transpose()
    }
    /// Borrow contiguous bytes starting at `offset` if already held by this source.
    /// This optional lookup must never start or wait for I/O. A miss falls back
    /// to the ordinary checked read when the caller needs those bytes.
    #[inline]
    fn retained_tail(&self, _offset: u64) -> Option<&[u8]> {
        None
    }
    #[cfg(all(feature = "mmap", target_os = "linux"))]
    #[doc(hidden)]
    fn mapped_file(&self) -> Option<&MappedFile> {
        None
    }
    /// Borrow a stable contiguous segment containing `offset`, when available.
    /// The tuple gives its absolute file offset and bytes. Each requested range
    /// within the segment must still be checked by the caller.
    #[inline]
    fn segment(&self, offset: u64) -> io::Result<Option<(u64, &[u8])>> {
        if let Some(bytes) = self.as_bytes() {
            checked_range(bytes, offset, 0)?;
            Ok(Some((0, bytes)))
        } else {
            Ok(None)
        }
    }
}

#[inline]
fn checked_range(bytes: &[u8], offset: u64, len: usize) -> io::Result<&[u8]> {
    let start = usize::try_from(offset).map_err(|_| io::ErrorKind::UnexpectedEof)?;
    let end = start.checked_add(len).ok_or(io::ErrorKind::UnexpectedEof)?;
    bytes
        .get(start..end)
        .ok_or_else(|| io::ErrorKind::UnexpectedEof.into())
}

impl Source for &[u8] {
    #[inline]
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    #[inline]
    fn as_bytes(&self) -> Option<&[u8]> {
        Some(self)
    }
    #[inline]
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        copy_at(self, offset, output)
    }
}
impl Source for Vec<u8> {
    #[inline]
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }
    #[inline]
    fn as_bytes(&self) -> Option<&[u8]> {
        Some(self)
    }
    #[inline]
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        copy_at(self, offset, output)
    }
}
fn copy_at(bytes: &[u8], offset: u64, output: &mut [u8]) -> io::Result<()> {
    output.copy_from_slice(checked_range(bytes, offset, output.len())?);
    Ok(())
}

/// Bounded-memory positioned I/O, with no mmap lifetime assumptions.
/// Fixed-width fields are read onto the stack; strings are returned owned.
pub struct FileSource {
    file: File,
    len: u64,
}
impl FileSource {
    /// Open a file for positioned reads.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::from_file(File::open(path)?)
    }
    /// Take ownership of an open file, retaining its access configuration.
    ///
    /// Length is captured at construction. Reads address the entire file,
    /// independently of its current cursor. The file must support positioned
    /// reads; contents must remain stable for consistent query results.
    pub fn from_file(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }
}
impl Source for FileSource {
    #[inline]
    fn len(&self) -> u64 {
        self.len
    }
    #[inline]
    fn read_exact_at(&self, mut offset: u64, mut output: &mut [u8]) -> io::Result<()> {
        while !output.is_empty() {
            #[cfg(unix)]
            let result = {
                use std::os::unix::fs::FileExt;
                self.file.read_at(output, offset)
            };
            #[cfg(windows)]
            let result = {
                use std::os::windows::fs::FileExt;
                self.file.seek_read(output, offset)
            };
            #[cfg(not(any(unix, windows)))]
            let result: io::Result<usize> = Err(io::ErrorKind::Unsupported.into());
            match result {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    offset += n as u64;
                    output = &mut output[n..];
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Positioned file I/O with reusable 4 KiB buffers for clustered decoder reads.
///
/// Buffers belong to individual iterators, not a shared file cache. They are
/// released with those iterators, and feature strings remain independently
/// owned. Direct `Source` reads and fields outside these windows use positioned
/// I/O. File contents must remain stable for a consistent query.
pub struct WindowedFile(FileSource);
impl WindowedFile {
    /// Open a file for positioned reads with iterator-owned read windows.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self(FileSource::open(path)?))
    }
    /// Take ownership of an open file, as in [`FileSource::from_file`].
    pub fn from_file(file: File) -> io::Result<Self> {
        Ok(Self(FileSource::from_file(file)?))
    }
}
impl Source for WindowedFile {
    #[inline]
    fn len(&self) -> u64 {
        self.0.len()
    }
    #[inline]
    fn read_buffer_size(&self) -> usize {
        4096
    }
    #[inline]
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        self.0.read_exact_at(offset, output)
    }
}

type Page = OnceLock<io::Result<Vec<u8>>>;
const PAGES_PER_GROUP: usize = 128;

/// Safe, lazy file reads into immutable 32 KiB pages.
///
/// Unlike file-backed mmap, external mutation cannot invalidate Rust references.
/// A consistent GOL still requires stable file contents while it is read.
/// Pages are retained until this source is dropped so returned borrows stay
/// valid. Memory is proportional to touched pages plus a small page directory;
/// use `FileSource` when a fixed memory bound is more important than throughput.
pub struct PagedFile {
    file: FileSource,
    directory: PageDirectory,
}

enum PageDirectory<P = Page> {
    Flat(Vec<P>),
    Sparse {
        groups: Vec<OnceLock<Box<[P]>>>,
        page_count: usize,
    },
}

fn empty_slots<T>(count: usize) -> io::Result<Vec<OnceLock<T>>> {
    let mut slots = Vec::new();
    slots.try_reserve_exact(count).map_err(io::Error::other)?;
    slots.resize_with(count, OnceLock::new);
    Ok(slots)
}

impl<P: Default> PageDirectory<P> {
    fn sparse(page_count: usize) -> io::Result<Self> {
        Ok(Self::Sparse {
            groups: empty_slots(page_count.div_ceil(PAGES_PER_GROUP))?,
            page_count,
        })
    }

    #[inline]
    fn slot<const LOAD: bool>(&self, index: usize) -> Option<&P> {
        match self {
            Self::Flat(pages) => pages.get(index),
            Self::Sparse { groups, page_count } => {
                let group_index = index / PAGES_PER_GROUP;
                let group = groups.get(group_index)?;
                let pages = if LOAD {
                    group.get_or_init(|| {
                        let count =
                            (page_count - group_index * PAGES_PER_GROUP).min(PAGES_PER_GROUP);
                        std::iter::repeat_with(P::default).take(count).collect()
                    })
                } else {
                    group.get()?
                };
                pages.get(index % PAGES_PER_GROUP)
            }
        }
    }
}

impl PagedFile {
    const PAGE_BYTES: usize = 32 * 1024;
    /// Open a file with lazily populated, retained pages.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::from_file(File::open(path)?)
    }
    /// Take ownership of an open file, as in [`FileSource::from_file`].
    /// Loaded pages are retained until this source is dropped.
    pub fn from_file(file: File) -> io::Result<Self> {
        Self::with_directory(file, false)
    }
    /// Allocate page slots in groups on first access. This reduces startup
    /// allocations for short queries over large files.
    pub fn from_file_sparse(file: File) -> io::Result<Self> {
        Self::with_directory(file, true)
    }
    fn with_directory(file: File, sparse: bool) -> io::Result<Self> {
        let file = FileSource::from_file(file)?;
        let page_count = usize::try_from(file.len().div_ceil(Self::PAGE_BYTES as u64))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file too large"))?;
        let directory = if sparse {
            PageDirectory::sparse(page_count)?
        } else {
            PageDirectory::Flat(empty_slots(page_count)?)
        };
        Ok(Self { file, directory })
    }
    /// Bytes retained in successfully loaded pages, excluding directory metadata.
    pub fn cached_bytes(&self) -> usize {
        let bytes = |pages: &[Page]| -> usize {
            pages
                .iter()
                .filter_map(|page| page.get()?.as_ref().ok())
                .map(Vec::len)
                .sum()
        };
        match &self.directory {
            PageDirectory::Flat(pages) => bytes(pages),
            PageDirectory::Sparse { groups, .. } => groups
                .iter()
                .filter_map(OnceLock::get)
                .map(|pages| bytes(pages))
                .sum(),
        }
    }
    /// Size of a cache page. The final page may be shorter.
    pub fn page_bytes(&self) -> usize {
        Self::PAGE_BYTES
    }
    #[inline]
    fn page(&self, index: usize) -> io::Result<&[u8]> {
        let page = self
            .directory
            .slot::<true>(index)
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        let value = page.get_or_init(|| {
            let offset = index as u64 * Self::PAGE_BYTES as u64;
            let len = (self.file.len() - offset).min(Self::PAGE_BYTES as u64) as usize;
            let mut data = vec![0; len];
            self.file.read_exact_at(offset, &mut data)?;
            Ok(data)
        });
        value
            .as_deref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))
    }
}
impl Source for PagedFile {
    #[inline]
    fn len(&self) -> u64 {
        self.file.len()
    }
    #[inline]
    fn retained_tail(&self, offset: u64) -> Option<&[u8]> {
        let index = usize::try_from(offset / Self::PAGE_BYTES as u64).ok()?;
        let within = (offset % Self::PAGE_BYTES as u64) as usize;
        let bytes = self.directory.slot::<false>(index)?.get()?.as_ref().ok()?;
        bytes.get(within..)
    }
    #[inline]
    fn segment(&self, offset: u64) -> io::Result<Option<(u64, &[u8])>> {
        if offset >= self.len() {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let index = (offset / Self::PAGE_BYTES as u64) as usize;
        Ok(Some((
            index as u64 * Self::PAGE_BYTES as u64,
            self.page(index)?,
        )))
    }
    #[inline]
    fn view(&self, offset: u64, len: usize) -> io::Result<Option<&[u8]>> {
        if offset
            .checked_add(len as u64)
            .is_none_or(|end| end > self.len())
        {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if len == 0 {
            return Ok(Some(&[]));
        }
        let index = (offset / Self::PAGE_BYTES as u64) as usize;
        let within = (offset % Self::PAGE_BYTES as u64) as usize;
        if len > Self::PAGE_BYTES - within {
            return Ok(None);
        }
        Ok(Some(checked_range(self.page(index)?, within as u64, len)?))
    }
    #[inline]
    fn read_exact_at(&self, mut offset: u64, mut output: &mut [u8]) -> io::Result<()> {
        if offset
            .checked_add(output.len() as u64)
            .is_none_or(|end| end > self.len())
        {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        while !output.is_empty() {
            let index = (offset / Self::PAGE_BYTES as u64) as usize;
            let within = (offset % Self::PAGE_BYTES as u64) as usize;
            let page = self.page(index)?;
            let count = output.len().min(page.len() - within);
            output[..count].copy_from_slice(&page[within..within + count]);
            output = &mut output[count..];
            offset += count as u64;
        }
        Ok(())
    }
}

#[cfg(feature = "mmap")]
/// Read-only mapping of an immutable GOL. On Linux, long prepared scans that
/// encounter cold pages can use a shared pool to prefetch upcoming tiles.
/// The iterator yields in traversal order. Dropping the source waits for active
/// helpers to finish accessing its mapping.
pub struct MappedFile {
    #[cfg(target_os = "linux")]
    map: std::sync::Arc<memmap2::Mmap>,
    #[cfg(not(target_os = "linux"))]
    map: memmap2::Mmap,
    #[cfg(target_os = "linux")]
    file: std::sync::Arc<File>,
    #[cfg(target_os = "linux")]
    helpers: Option<mmap_read_ahead::HelperOwner>,
}
#[cfg(feature = "mmap")]
impl MappedFile {
    /// Map a stable file without copying its contents.
    ///
    /// # Safety
    /// The caller must ensure that no process modifies or truncates the backing
    /// file until this mapping is dropped. A read-only handle alone does not
    /// establish that guarantee. Use `FileSource` when it cannot be established.
    #[allow(unsafe_code)]
    pub unsafe fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        // SAFETY: The caller keeps this file immutable for the mapping lifetime.
        unsafe { Self::map_immutable_file(file) }
    }

    /// Map the backing file of an existing opened handle without copying it.
    ///
    /// # Safety
    /// The caller must ensure that no process modifies or truncates the backing
    /// file until this mapping is dropped.
    #[allow(unsafe_code)]
    pub unsafe fn from_immutable_file(file: &File) -> io::Result<Self> {
        // SAFETY: The duplicate refers to the same immutable backing file.
        unsafe { Self::map_immutable_file(file.try_clone()?) }
    }

    #[allow(unsafe_code)]
    unsafe fn map_immutable_file(file: File) -> io::Result<Self> {
        // SAFETY: The caller guarantees that the backing file stays immutable
        // throughout the mapping's lifetime, as required by memmap2.
        let map = unsafe { memmap2::MmapOptions::new().map(&file)? };
        #[cfg(target_os = "linux")]
        let map = std::sync::Arc::new(map);
        Ok(Self {
            map,
            #[cfg(target_os = "linux")]
            file: std::sync::Arc::new(file),
            #[cfg(target_os = "linux")]
            helpers: Some(mmap_read_ahead::HelperOwner::default()),
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn worker_library(&self) -> Option<std::sync::Weak<mmap_read_ahead::WorkerMap>> {
        let owner = self.helpers.as_ref()?;
        owner.access(|| {
            let source = Self {
                map: self.map.clone(),
                file: self.file.clone(),
                helpers: None,
            };
            crate::Library::with_dictionary_loading(
                source,
                crate::Limits::default(),
                crate::DictionaryLoading::OnDemand,
            )
            .ok()
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn file(&self) -> &File {
        &self.file
    }
}
#[cfg(feature = "mmap")]
impl Source for MappedFile {
    #[inline]
    fn len(&self) -> u64 {
        self.map.len() as u64
    }
    #[inline]
    fn as_bytes(&self) -> Option<&[u8]> {
        Some(&self.map)
    }
    #[cfg(target_os = "linux")]
    #[inline]
    fn mapped_file(&self) -> Option<&MappedFile> {
        Some(self)
    }
    #[inline]
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        copy_at(&self.map, offset, output)
    }
}

#[cfg(all(feature = "mmap", target_os = "linux"))]
mod mmap_read_ahead;
#[cfg(all(feature = "mmap", feature = "parallel", target_os = "linux"))]
pub(crate) use mmap_read_ahead::FaultSample;
#[cfg(all(feature = "mmap", target_os = "linux"))]
pub(crate) use mmap_read_ahead::{MappedScan, WorkerMap};

#[cfg(feature = "http")]
mod http;
#[cfg(feature = "http")]
pub use http::HttpSource;

mod cache;
pub use cache::CachedFile;

#[cfg(all(test, feature = "mmap", target_os = "linux"))]
pub(crate) use mmap_read_ahead::test_support as mapped_scan_test_support;
