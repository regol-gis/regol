use regol::{
    DictionaryLoading, Library, Limits,
    source::{HttpSource, MappedFile, PagedFile},
};
use std::{error::Error, ffi::OsStr, path::Path};

pub(super) struct Options {
    pub(super) mmap: bool,
    pub(super) dictionary: DictionaryLoading,
    http_page_bytes: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mmap: true,
            dictionary: DictionaryLoading::OnDemand,
            http_page_bytes: None,
        }
    }
}

pub(super) enum Input {
    Http(Box<Library<HttpSource>>),
    Mmap(Library<MappedFile>),
    File(Library<PagedFile>),
}

impl Options {
    pub(super) fn set_http_page_kib(&mut self, value: &OsStr) -> Result<(), &'static str> {
        let bytes = value
            .to_str()
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|n| n.checked_mul(1024))
            .ok_or("--http-page-kib requires a size in KiB")?;
        self.http_page_bytes = Some(bytes);
        Ok(())
    }

    pub(super) fn open(&self, path: &Path) -> Result<Input, Box<dyn Error>> {
        if let Some(url) = http_url(path.as_os_str()) {
            let source = match self.http_page_bytes {
                Some(size) => HttpSource::with_page_size(url, size)?,
                None => HttpSource::open(url)?,
            };
            Ok(Input::Http(Box::new(Library::with_dictionary_loading(
                source,
                Limits::default(),
                self.dictionary,
            )?)))
        } else if self.mmap {
            // SAFETY: Both query commands require the GOL file to remain immutable
            // for the lifetime of the command unless --no-mmap is specified.
            let source = unsafe { MappedFile::open(path)? };
            Ok(Input::Mmap(Library::with_dictionary_loading(
                source,
                Limits::default(),
                self.dictionary,
            )?))
        } else {
            Ok(Input::File(Library::open_with_dictionary_loading(
                path,
                self.dictionary,
            )?))
        }
    }
}

pub(super) fn http_url(file: &OsStr) -> Option<&str> {
    let value = file.to_str()?;
    let (scheme, _) = value.split_once("://")?;
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")).then_some(value)
}
