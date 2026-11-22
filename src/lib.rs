#![deny(unsafe_code)]
#![cfg_attr(not(feature = "mmap"), forbid(unsafe_code))]
#![doc = include_str!("README.md")]

mod adaptive_text_index;
#[cfg(feature = "build")]
pub mod build;
mod coordinates;
mod decode;
mod error;
mod feature;
mod feature_set;
mod geometry;
#[cfg(feature = "gob")]
pub mod gob;
mod inspect;
mod library;
mod links;
mod measurements;
#[cfg(feature = "parallel")]
mod parallel;
mod query;
pub mod source;
mod spatial;
mod spatial_filter;
#[cfg(all(unix, feature = "experimental-tag-sidecars"))]
pub mod tag_sidecar;
mod text_index;
pub mod units;
#[cfg(feature = "way-checkpoints")]
mod way_checkpoints;

pub use coordinates::{Latitude, LonLat, Longitude};
pub use error::{Error, Result};
pub use feature::{
    Coordinates, Feature, FeatureId, FeatureType, Member, Members, Tag, TagValue, Tags,
};
pub use feature_set::FeatureSet;
pub use geometry::{Geometry, Polygon};
pub use inspect::IntegrityReport;
pub use library::{DictionaryLoading, Library, Limits, Properties};
pub use links::{FeatureLinks, ParentWays, WayNode, WayNodes};
#[cfg(feature = "parallel")]
pub use parallel::{QueryExecutor, QueryResults};
pub use query::{Predicate, PreparedQuery, Query, QueryIter, Selector, Types};
pub use spatial::{Bounds, Features, Point};
pub use spatial_filter::{SpatialFilter, SpatialPredicate};
#[cfg(feature = "way-checkpoints")]
pub use way_checkpoints::{OriginalWayNodeIds, WayCheckpointIndex};
