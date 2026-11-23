use crate::geometry::{GeoGeometry, coordinates};
use regol::{Error, Feature, FeatureType, source::Source};

pub(crate) fn center<S: Source>(feature: Feature<'_, S>) -> regol::Result<GeoGeometry> {
    let [west, south, east, north] = if feature.kind() == FeatureType::Relation {
        relation_bounds(feature)?
    } else {
        node_or_way_bounds(feature)?
    };
    let lon = (west + east) * 0.5;
    let lon = if west <= east {
        lon
    } else if lon <= 0.0 {
        lon + 180.0
    } else {
        lon - 180.0
    };
    Ok(GeoGeometry::Point {
        coordinates: [lon, (south + north) * 0.5],
    })
}

fn node_or_way_bounds<S: Source>(feature: Feature<'_, S>) -> regol::Result<[f64; 4]> {
    let bounds = feature.bounds()?;
    let [west, south] = coordinates(&bounds.min);
    let [east, north] = coordinates(&bounds.max);
    if east - west <= 180.0 {
        return Ok([west, south, east, north]);
    }

    // The stored Mercator box cannot distinguish a dateline crossing from a
    // path that spans the world. Inspect vertices only for that ambiguous case.
    let (mut west, mut east) = (180.0_f64, -180.0_f64);
    for point in feature.coordinates()? {
        let [lon, _] = coordinates(&point?);
        if lon > 0.0 {
            west = west.min(lon);
        } else {
            east = east.max(lon);
        }
    }
    if west - east <= 180.0 {
        (west, east) = (-180.0, 180.0);
    }
    Ok([west, south, east, north])
}

fn relation_bounds<S: Source>(feature: Feature<'_, S>) -> regol::Result<[f64; 4]> {
    let (mut west, mut south, mut east, mut north) = (180.0_f64, 90.0_f64, -180.0_f64, -90.0_f64);
    let (mut wrapped_west, mut wrapped_east) = (180.0_f64, -180.0_f64);
    let (mut wraps, mut can_wrap) = (false, true);
    for member in feature.members()? {
        let member = member?.feature;
        if member.kind() == FeatureType::Relation {
            continue;
        }
        let [w, s, e, n] = node_or_way_bounds(member)?;
        west = west.min(w);
        south = south.min(s);
        east = east.max(e);
        north = north.max(n);
        wraps |= w > e;
        can_wrap &= e <= 0.0 || w >= 0.0;
        if e <= 0.0 {
            wrapped_east = wrapped_east.max(e);
        }
        if w >= 0.0 {
            wrapped_west = wrapped_west.min(w);
        }
    }
    if south > north {
        return Err(Error::Geometry(
            "relation has no direct node or way members",
        ));
    }
    if wraps || east - west > 180.0 {
        (west, east) = if can_wrap && wrapped_west - wrapped_east > 180.0 {
            (wrapped_west, wrapped_east)
        } else {
            (-180.0, 180.0)
        };
    }
    Ok([west, south, east, north])
}
