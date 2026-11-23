pub(super) type Window = [f64; 4];

const DEGREES_TO_RADIANS: f64 = std::f64::consts::PI / 180.0;

fn split(west: f64, south: f64, east: f64, north: f64) -> Vec<Window> {
    let south = south.max(-90.0);
    let north = north.min(90.0);
    let width = east - west;
    if width >= 360.0 {
        return vec![[-180.0, south, 180.0, north]];
    }
    let start = (((west + 180.0) % 360.0 + 360.0) % 360.0) - 180.0;
    let end = start + width;
    if end <= 180.0 {
        vec![[start, south, end, north]]
    } else {
        vec![
            [start, south, 180.0, north],
            [-180.0, south, end - 360.0, north],
        ]
    }
}

pub(super) fn expanded(windows: &[Window], meters: f64) -> Vec<Window> {
    let angle = (meters / 6_371_000.0 + 1e-8).min(std::f64::consts::PI);
    compact(windows.iter().flat_map(|&[west, south, east, north]| {
        let lower = (-90.0_f64).max(south - angle / DEGREES_TO_RADIANS);
        let upper = (90.0_f64).min(north + angle / DEGREES_TO_RADIANS);
        if lower == -90.0 || upper == 90.0 {
            vec![[-180.0, lower, 180.0, upper]]
        } else {
            let ratio = angle.sin() / ((-south).max(north) * DEGREES_TO_RADIANS).cos();
            let dx = ratio.min(1.0).asin() / DEGREES_TO_RADIANS;
            split(west - dx, lower, east + dx, upper)
        }
    }))
}

pub(super) fn polygon(points: &[[f64; 2]]) -> Option<Vec<Window>> {
    let [mut west, mut south, mut east, mut north] = [180.0_f64, 90.0_f64, -180.0_f64, -90.0_f64];
    let mut shift = 0.0_f64;
    let mut previous = points[0][0];
    for &[lon, lat] in points.iter().chain(points.first()) {
        let delta = lon - previous;
        if delta.abs() > 180.0 {
            shift -= delta.signum() * 360.0;
        }
        west = west.min(lon + shift);
        east = east.max(lon + shift);
        south = south.min(lat);
        north = north.max(lat);
        previous = lon;
    }
    if shift.abs() == 360.0 {
        let lower = if south > 0.0 { south } else { -90.0 };
        let upper = if north < 0.0 { north } else { 90.0 };
        return Some(vec![[-180.0, lower, 180.0, upper]]);
    }
    if shift != 0.0 {
        return None;
    }
    // Both longitude encodings of the seam belong to the search.
    if west >= -180.0 && east <= 180.0 {
        let mut windows = vec![[west, south, east, north]];
        if east == 180.0 {
            windows.push([-180.0, south, -180.0, north]);
        }
        if west == -180.0 {
            windows.push([180.0, south, 180.0, north]);
        }
        return Some(windows);
    }
    Some(split(west, south, east, north))
}

pub(super) fn compact(windows: impl IntoIterator<Item = Window>) -> Vec<Window> {
    // Keep distinct windows in traversal order unless their number warrants
    // switching to the envelope. That envelope already includes duplicates.
    const MAX_WINDOWS: usize = 32;
    let mut distinct = Vec::new();
    let mut extent: Option<Window> = None;
    for current in windows {
        if let Some([west, south, east, north]) = &mut extent {
            *west = west.min(current[0]);
            *south = south.min(current[1]);
            *east = east.max(current[2]);
            *north = north.max(current[3]);
        } else {
            extent = Some(current);
        }
        if distinct.len() <= MAX_WINDOWS && !distinct.contains(&current) {
            distinct.push(current);
        }
    }
    if distinct.len() <= MAX_WINDOWS {
        distinct
    } else {
        extent.into_iter().collect()
    }
}
