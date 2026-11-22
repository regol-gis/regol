use std::{fmt, io};

/// A result from reading, querying or decoding a GOL.
pub type Result<T> = std::result::Result<T, Error>;

/// An I/O, format, query or resource-limit error returned by the reader.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The underlying source could not complete an I/O operation.
    Io(io::Error),
    /// A field or reference violates the GOL format.
    Invalid {
        /// Byte offset in the GOL associated with the invalid data.
        offset: u64,
        /// Description of the failed format check.
        message: &'static str,
    },
    /// The header contains an unsupported format version.
    UnsupportedVersion(u32),
    /// A Geo-Object Bundle catalog, metadata section or chunk is invalid.
    #[cfg(feature = "gob")]
    InvalidGob {
        /// Byte offset associated with the invalid data.
        offset: u64,
        /// Description of the failed format check.
        message: &'static str,
    },
    /// A referenced tile is missing or unavailable in the active snapshot.
    MissingTile(u32),
    /// A configured resource ceiling was reached.
    Limit(&'static str),
    /// The operation requires a different kind of feature.
    WrongFeatureType,
    /// A set operation combined handles from different open libraries.
    DifferentLibraries,
    /// A GOQL selector could not be parsed or validated.
    Query {
        /// Byte offset in the query associated with the error.
        offset: usize,
        /// Explanation of the rejected query.
        message: String,
    },
    /// Geometry could not be assembled from the stored records.
    Geometry(&'static str),
    /// The way checkpoint index could not be read or used.
    #[cfg(feature = "way-checkpoints")]
    WayCheckpoints(&'static str),
}

impl Error {
    pub(crate) fn invalid(offset: u64, message: &'static str) -> Self {
        Self::Invalid { offset, message }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Invalid { offset, message } => write!(f, "invalid GOL at {offset:#x}: {message}"),
            Self::UnsupportedVersion(v) => {
                write!(f, "unsupported GOL format version {v}; expected 2")
            }
            #[cfg(feature = "gob")]
            Self::InvalidGob { offset, message } => {
                write!(f, "invalid GOB at {offset:#x}: {message}")
            }
            Self::MissingTile(tip) => write!(f, "tile {tip} is missing or not current"),
            Self::Limit(s) => write!(f, "resource limit exceeded: {s}"),
            Self::WrongFeatureType => {
                f.write_str("operation is not available for this feature type")
            }
            Self::DifferentLibraries => f.write_str("features belong to different open libraries"),
            Self::Query { offset, message } => write!(f, "query error at byte {offset}: {message}"),
            Self::Geometry(message) => write!(f, "geometry error: {message}"),
            #[cfg(feature = "way-checkpoints")]
            Self::WayCheckpoints(message) => write!(f, "way checkpoints: {message}"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
