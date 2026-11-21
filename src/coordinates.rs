use crate::{
    Bounds, Error, Point, Result,
    units::{
        Angle,
        angle::{Conversion, degree, radian},
    },
};
use std::f64::consts::{FRAC_PI_2, PI, TAU};

// The GOL projection spans slightly less than 2^32 - 1 grid steps.
pub(crate) const WORLD_WIDTH: f64 = 4_294_967_294.999_9;

macro_rules! coordinate_angle {
    ($(#[$attr:meta])* $name:ident, $limit:expr, $message:literal) => {
        $(#[$attr])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
        pub struct $name(Angle);

        impl $name {
            /// Construct a coordinate in the specified angular unit.
            pub fn new<U: Conversion<f64>>(value: f64) -> Result<Self> {
                Angle::new::<U>(value).try_into()
            }

            /// Read the coordinate in the specified angular unit.
            pub fn get<U: Conversion<f64>>(self) -> f64 {
                self.0.get::<U>()
            }
        }

        impl TryFrom<Angle> for $name {
            type Error = Error;

            fn try_from(angle: Angle) -> Result<Self> {
                if (-$limit..=$limit).contains(&angle.get::<radian>()) {
                    Ok(Self(angle))
                } else {
                    Err(Error::Geometry($message))
                }
            }
        }

        impl From<$name> for Angle {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

coordinate_angle!(
    /// Longitude east of Greenwich, from −180° through +180° inclusive.
    Longitude,
    PI,
    "longitude must be finite and within [-180, 180] degrees"
);
coordinate_angle!(
    /// Latitude north of the equator, from −90° through +90° inclusive.
    Latitude,
    FRAC_PI_2,
    "latitude must be finite and within [-90, 90] degrees"
);

/// Geographic coordinates with distinct, validated axes.
///
/// ```
/// use regol::{Latitude, Longitude, LonLat, units::angle::degree};
/// let position = LonLat {
///     longitude: Longitude::new::<degree>(-2.59)?,
///     latitude: Latitude::new::<degree>(51.45)?,
/// };
/// let grid = position.project();
/// assert!((grid.lon_lat().latitude.get::<degree>() - 51.45).abs() < 1e-7);
/// # Ok::<(), regol::Error>(())
/// ```
///
/// ```compile_fail
/// use regol::{Latitude, Longitude, LonLat, units::angle::degree};
/// let latitude = Latitude::new::<degree>(51.45).unwrap();
/// let longitude = Longitude::new::<degree>(-2.59).unwrap();
/// let swapped = LonLat { longitude: latitude, latitude: longitude };
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LonLat {
    /// Longitude east of Greenwich.
    pub longitude: Longitude,
    /// Latitude north of the equator.
    pub latitude: Latitude,
}

impl LonLat {
    /// Project to the nearest GOL grid point, with half steps rounded upward.
    /// Latitudes beyond the grid's Mercator extent saturate at its north or south edge.
    pub fn project(self) -> Point {
        let x = self.longitude.get::<degree>() * WORLD_WIDTH / 360.0;
        let y = self.latitude.get::<radian>().tan().asinh() * WORLD_WIDTH / TAU;
        Point {
            x: (x + 0.5).floor() as i32,
            y: (y + 0.5).floor() as i32,
        }
    }
}

impl Point {
    /// Geographic position at the centre of this integer Mercator grid cell.
    /// The westernmost cell centre falls just west of −180° and is reported
    /// as −180°. Projecting that longitude back selects the adjacent cell.
    pub fn lon_lat(self) -> LonLat {
        let (lon, lat) = self.radians();
        LonLat {
            longitude: Longitude(Angle::new::<radian>(lon.max(-PI))),
            latitude: Latitude(Angle::new::<radian>(lat)),
        }
    }

    // Geometry calculations retain the exact grid centre even at the antimeridian.
    pub(crate) fn radians(self) -> (f64, f64) {
        let lon = f64::from(self.x) * (TAU / WORLD_WIDTH);
        let lat = (f64::from(self.y) * (TAU / WORLD_WIDTH)).sinh().atan();
        (lon, lat)
    }
}

impl Bounds {
    /// Construct an inclusive GOL query window from its geographic corners.
    /// Checks ordering before rounding onto the grid. A window crossing the
    /// antimeridian must be split into western and eastern windows.
    pub fn from_lon_lat(southwest: LonLat, northeast: LonLat) -> Result<Self> {
        if southwest.longitude > northeast.longitude || southwest.latitude > northeast.latitude {
            return Err(Error::Geometry("geographic bbox is inverted"));
        }
        let mut min = southwest.project();
        // Both this cell and its neighbour are reached when querying the western edge.
        if southwest.longitude.get::<radian>() == -PI {
            min.x = i32::MIN;
        }
        Ok(Self {
            min,
            max: northeast.project(),
        })
    }
}
