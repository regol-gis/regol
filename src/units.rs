//! Physical measurements use the standard `uom` SI system with `f64` storage.
//! Use the unit modules here to construct a quantity or obtain a value for display.
//!
//! ```
//! use regol::{SpatialPredicate, units::{Length, length::{kilometer, meter}}};
//!
//! let radius = Length::new::<kilometer>(0.5);
//! let filter = SpatialPredicate::WithinDistance(radius);
//! assert_eq!(radius.get::<meter>(), 500.0);
//! ```
//!
//! ```compile_fail
//! use regol::{SpatialPredicate, units::{Area, area::hectare}};
//! let filter = SpatialPredicate::WithinDistance(Area::new::<hectare>(1.0));
//! ```
pub use uom::si::{
    angle, area,
    f64::{Angle, Area, Length},
    length,
};
