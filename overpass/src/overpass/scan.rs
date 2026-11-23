use regol::{Bounds, Feature, FeatureType, Point, PreparedQuery, source::Source};
use std::{collections::BTreeMap, f64::consts::TAU};

pub(super) const WORLD_WIDTH: i64 = (1 << 32) - 1;
pub(super) const HALF_WORLD: u32 = (WORLD_WIDTH / 2) as u32;

// Choose widths in the stored grid, with exact whole-grid shifts at half width.
const LOCAL_WIDTH: u32 = 1 << 23; // about 0.7°
const WIDTHS: [u32; 4] = [LOCAL_WIDTH, 3 << 24, 3 << 26, 3 << 28]; // 0.7°, 4.2°, 16.9°, 67.5°
const MAX_TIER_WINDOWS: usize = 128;
const NODE_AREA_SHIFT: u32 = 24;
const NODE_AREA_CELL: u32 = 1 << NODE_AREA_SHIFT;
const MAX_EXTRA_NODE_AREA_WINDOWS: usize = 256;

/// Reset the reader's traversal budget at tile-aligned boundaries for large
/// point-only area scans. Every projected coordinate belongs to one partition
/// per input window, so partitioning cannot duplicate a node.
pub(crate) fn node_area_windows(
    windows: &[Bounds],
    check: &impl Fn() -> Result<(), String>,
) -> Result<Vec<Bounds>, String> {
    let mut split = Vec::with_capacity(windows.len());
    let mut remaining = MAX_EXTRA_NODE_AREA_WINDOWS;
    for &bounds in windows {
        check()?;
        let left = bounds.min.x >> NODE_AREA_SHIFT;
        let right = bounds.max.x >> NODE_AREA_SHIFT;
        let bottom = bounds.min.y >> NODE_AREA_SHIFT;
        let top = bounds.max.y >> NODE_AREA_SHIFT;
        let extra = ((right - left + 1) * (top - bottom + 1)) as usize - 1;
        if bounds.min.x.abs_diff(bounds.max.x) < NODE_AREA_CELL
            && bounds.min.y.abs_diff(bounds.max.y) < NODE_AREA_CELL
            || extra > remaining
        {
            // Keep the original scan for very broad windows. A sparse
            // selector can cover them within its traversal budget already.
            split.push(bounds);
            continue;
        }
        remaining -= extra;
        for y in bottom..=top {
            let min_y = y << NODE_AREA_SHIFT;
            let max_y = min_y | (NODE_AREA_CELL - 1) as i32;
            for x in left..=right {
                check()?;
                let min_x = x << NODE_AREA_SHIFT;
                let max_x = min_x | (NODE_AREA_CELL - 1) as i32;
                split.push(Bounds {
                    min: Point {
                        x: min_x.max(bounds.min.x),
                        y: min_y.max(bounds.min.y),
                    },
                    max: Point {
                        x: max_x.min(bounds.max.x),
                        y: max_y.min(bounds.max.y),
                    },
                });
            }
        }
    }
    Ok(split)
}

pub(crate) struct CandidateScan {
    tiers: Vec<Tier>,
    around: bool,
}

struct Tier {
    windows: Vec<Bounds>,
    min_width: u32,
    max_width: u32,
}

impl CandidateScan {
    pub(crate) fn around(local: &mut [Bounds]) -> Self {
        let mut tiers = Vec::new();
        for pair in WIDTHS.windows(2) {
            tiers.push(arc_tier(local, pair[0], pair[1]));
        }
        let widest = WIDTHS[WIDTHS.len() - 1];
        tiers.push(arc_tier(local, widest, HALF_WORLD));
        if let Some(window) = local.first() {
            // A box wider than half the world covers at least one of any
            // pair of opposite meridians. Probe away from the local search.
            let meridian = window.min.x.midpoint(window.max.x);
            let band = Bounds {
                min: Point {
                    x: meridian,
                    ..Bounds::WORLD.min
                },
                max: Point {
                    x: meridian,
                    ..Bounds::WORLD.max
                },
            };
            let mut windows = Vec::new();
            shifted_band(band, -WORLD_WIDTH / 4, &mut windows);
            shifted_band(band, WORLD_WIDTH / 4, &mut windows);
            tiers.push(Tier {
                windows,
                min_width: HALF_WORLD,
                max_width: u32::MAX,
            });
        }

        let latitude_factor = arc_latitude_factor(LOCAL_WIDTH);
        for window in local {
            widen_latitude(window, latitude_factor);
        }
        Self {
            tiers,
            around: true,
        }
    }

    pub(crate) fn planar(local: &[Bounds]) -> Option<Self> {
        let mut windows = Vec::new();
        for window in local {
            if window.min.x <= 0 && window.max.x >= 0 {
                // A stored box with a wrapped segment always spans Greenwich.
                continue;
            }
            let midpoint = window.min.x.midpoint(window.max.x);
            let center = Bounds {
                min: Point {
                    x: midpoint,
                    ..window.min
                },
                max: Point {
                    x: midpoint,
                    ..window.max
                },
            };
            // If a wrapped path reaches the window without intersecting its
            // stored box, that window lies outside the stored X range. Its
            // midpoint's opposite longitude then lies inside the range.
            shifted_band(center, WORLD_WIDTH / 2, &mut windows);
        }
        (!windows.is_empty()).then(|| Self {
            tiers: vec![Tier {
                windows: merge_bands(windows),
                min_width: HALF_WORLD,
                max_width: u32::MAX,
            }],
            around: false,
        })
    }

    pub(crate) fn is_local<S: Source>(&self, feature: Feature<'_, S>) -> regol::Result<bool> {
        if !self.around || feature.kind() == FeatureType::Node {
            return Ok(true);
        }
        let bounds = feature.bounds()?;
        Ok(bounds.min.x.abs_diff(bounds.max.x) <= LOCAL_WIDTH)
    }

    pub(crate) fn wide_candidates<'a, 'b, S: Source>(
        &'b self,
        query: &'b PreparedQuery<'a, '_, S>,
        local: &'b [Bounds],
        check: &'b impl Fn() -> Result<(), String>,
    ) -> impl Iterator<Item = regol::Result<Feature<'a, S>>> + 'b
    where
        'a: 'b,
    {
        self.tiers.iter().flat_map(move |tier| {
            tier.windows
                .iter()
                .enumerate()
                .flat_map(move |(index, &window)| {
                    // Check between scans even when a selector yields no features.
                    let begin = check()
                        .err()
                        .map(|message| Err(std::io::Error::other(message).into()));
                    begin.into_iter().chain(
                        query
                            .candidates_in_bounds_with_min_width(window, tier.min_width + 1)
                            .filter_map(move |candidate| {
                                let result = (|| {
                                    check().map_err(std::io::Error::other)?;
                                    let feature = candidate?;
                                    let bounds = feature.bounds()?;
                                    let width = bounds.min.x.abs_diff(bounds.max.x);
                                    let accepted = tier.contains_width(width)
                                        && (self.around
                                            || !local
                                                .iter()
                                                .any(|window| window.intersects(bounds)))
                                        && !tier.windows[..index]
                                            .iter()
                                            .any(|window| window.intersects(bounds));
                                    Ok(accepted.then_some(feature))
                                })();
                                result.transpose()
                            }),
                    )
                })
        })
    }
}

impl Tier {
    fn contains_width(&self, width: u32) -> bool {
        width > self.min_width && width <= self.max_width
    }
}

fn arc_latitude_factor(width: u32) -> f64 {
    // A shorter great-circle arc is a normalized positive blend of its
    // endpoints. At a longitude difference ≤ 2α, the horizontal length is
    // at least cos(α) times the blend of horizontal lengths. Thus the
    // absolute tangent of latitude grows by at most 1/cos(α).
    let half_angle = f64::from(width) * TAU / (2.0 * WORLD_WIDTH as f64);
    half_angle.cos()
}

fn widen_latitude(window: &mut Bounds, latitude_factor: f64) {
    // An arc between two endpoints in one hemisphere cannot approach the
    // equator more closely than both endpoints. Allow for bulge by moving
    // just the equator-facing edge using Mercator Y = asinh(tan(latitude)).
    let scale = TAU / WORLD_WIDTH as f64;
    let closer_to_equator =
        |y: i32| ((f64::from(y) * scale).sinh() * latitude_factor).asinh() / scale;
    if window.min.y > 0 {
        window.min.y = (closer_to_equator(window.min.y).floor() as i32).saturating_sub(2);
    }
    if window.max.y < 0 {
        window.max.y = (closer_to_equator(window.max.y).ceil() as i32).saturating_add(2);
    }
}

fn arc_tier(local: &[Bounds], min_width: u32, max_width: u32) -> Tier {
    let mut windows = Vec::new();
    let latitude_factor = (max_width < HALF_WORLD).then(|| arc_latitude_factor(max_width));
    let shift = i64::from(min_width / 2);
    for &window in local {
        let mut band = window;
        if let Some(latitude_factor) = latitude_factor {
            widen_latitude(&mut band, latitude_factor);
        } else {
            band.min.y = Bounds::WORLD.min.y;
            band.max.y = Bounds::WORLD.max.y;
        }
        // A stored box wider than twice the shift must cover at least one
        // shifted longitude if it covers the original.
        shifted_band(band, -shift, &mut windows);
        shifted_band(band, shift, &mut windows);
    }
    Tier {
        windows: merge_bands(windows),
        min_width,
        max_width,
    }
}

fn merge_bands(mut windows: Vec<Bounds>) -> Vec<Bounds> {
    windows.sort_unstable_by_key(|window| (window.min.y, window.max.y, window.min.x));
    let mut merged: Vec<Bounds> = Vec::new();
    for window in windows {
        if let Some(last) = merged.last_mut()
            && window.min.y == last.min.y
            && window.max.y == last.max.y
            && window.min.x <= last.max.x.saturating_add(1)
        {
            last.max.x = last.max.x.max(window.max.x);
        } else {
            merged.push(window);
        }
    }
    // Keep small searches narrow. Long reference corridors can create many
    // overlapping bands with slightly different latitudes, so combine their
    // envelopes by grid cell before scanning the same tiles repeatedly.
    for shift in 20..=32 {
        if merged.len() <= MAX_TIER_WINDOWS {
            break;
        }
        let mut cells = BTreeMap::<(u32, u32), Bounds>::new();
        for window in merged {
            let cell = |min: i32, max: i32| {
                let shifted = i64::from(min.midpoint(max)) - i64::from(i32::MIN);
                (shifted >> shift) as u32
            };
            cells
                .entry((
                    cell(window.min.x, window.max.x),
                    cell(window.min.y, window.max.y),
                ))
                .and_modify(|bounds| {
                    bounds.min.x = bounds.min.x.min(window.min.x);
                    bounds.min.y = bounds.min.y.min(window.min.y);
                    bounds.max.x = bounds.max.x.max(window.max.x);
                    bounds.max.y = bounds.max.y.max(window.max.y);
                })
                .or_insert(window);
        }
        merged = cells.into_values().collect();
    }
    merged
}

fn shifted_band(window: Bounds, shift: i64, bands: &mut Vec<Bounds>) {
    // Allow for endpoint quantization and for the rounded half-world shift.
    let width = i64::from(window.max.x) - i64::from(window.min.x) + 4;
    let mut push = |min, max| {
        bands.push(Bounds {
            min: Point {
                x: min,
                ..window.min
            },
            max: Point {
                x: max,
                ..window.max
            },
        });
    };
    if width >= WORLD_WIDTH {
        push(i32::MIN, i32::MAX);
        return;
    }
    let left = (i64::from(window.min.x) + shift - 2 - i64::from(i32::MIN)).rem_euclid(WORLD_WIDTH)
        + i64::from(i32::MIN);
    let right = left + width;
    if right >= i64::from(i32::MAX) {
        push(left as i32, i32::MAX);
        push(i32::MIN, (right - WORLD_WIDTH) as i32);
    } else {
        push(left as i32, right as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry;

    #[test]
    fn node_area_partitions_cover_every_grid_point_exactly_once() {
        let cell = NODE_AREA_CELL as i32;
        let bounds = Bounds::new(-cell - 2, -3, cell + 5, cell + 6).unwrap();
        let split = node_area_windows(&[bounds], &|| Ok(())).unwrap();
        assert_eq!(split.len(), 12);
        let cells = |window: &Bounds| {
            (u64::from(window.min.x.abs_diff(window.max.x)) + 1)
                * (u64::from(window.min.y.abs_diff(window.max.y)) + 1)
        };
        for (i, window) in split.iter().enumerate() {
            assert!(bounds.contains(window.min) && bounds.contains(window.max));
            assert!(
                split[i + 1..]
                    .iter()
                    .all(|other| !window.intersects(*other))
            );
        }
        assert_eq!(split.iter().map(cells).sum::<u64>(), cells(&bounds));
        let small = Bounds::new(-1, -1, 3, 3).unwrap();
        assert_eq!(node_area_windows(&[small], &|| Ok(())).unwrap(), [small]);
    }

    #[test]
    fn node_area_partitions_are_bounded_and_cancellable() {
        assert_eq!(
            node_area_windows(&[Bounds::WORLD], &|| Ok(())).unwrap(),
            [Bounds::WORLD]
        );
        assert_eq!(
            node_area_windows(&[Bounds::WORLD], &|| Err("cancelled".into())).unwrap_err(),
            "cancelled"
        );
    }

    #[test]
    fn compacted_corridors_cover_each_original_band() {
        let original: Vec<_> = (0..1024)
            .map(|i| {
                let x = i * 2_000_000;
                let y = -i * 1_500_000;
                Bounds::new(x, y, x + 500_000, y + 800_000).unwrap()
            })
            .collect();
        let merged = merge_bands(original.clone());
        assert!(merged.len() <= MAX_TIER_WINDOWS);
        assert!(original.iter().all(|old| {
            merged
                .iter()
                .any(|new| new.contains(old.min) && new.contains(old.max))
        }));
    }

    #[test]
    fn arc_midpoints_are_found_across_width_tiers_and_the_seam() {
        let project = |lon, lat| geometry::geographic_point([lon, lat]).unwrap().project();
        for width in [0.6_f64, 2.0, 10.0, 40.0, 100.0, 170.0, 190.0, 320.0] {
            for lat in [-45.0_f64, 45.0] {
                let west = project(-width / 2.0, lat);
                let east = project(width / 2.0, lat);
                let stored = Bounds {
                    min: west,
                    max: east,
                };
                // Equal-latitude endpoint vectors sum to a vector with this
                // latitude and longitude 0° or 180°, depending on the sign
                // of the horizontal component.
                let mid_lat = (lat.to_radians().tan() / (width / 2.0).to_radians().cos().abs())
                    .atan()
                    .to_degrees();
                let mid_lon = if width < 180.0 { 0.0 } else { 180.0 };
                let mut windows =
                    [geometry::project_box([mid_lon, mid_lat, mid_lon, mid_lat]).unwrap()];
                assert!(!windows[0].intersects(stored));
                let scan = CandidateScan::around(&mut windows);
                let width_cells = west.x.abs_diff(east.x);
                let found = if width_cells <= LOCAL_WIDTH {
                    windows[0].intersects(stored)
                } else {
                    scan.tiers.iter().any(|tier| {
                        tier.contains_width(width_cells)
                            && tier.windows.iter().any(|window| window.intersects(stored))
                    })
                };
                assert!(
                    found,
                    "missing arc midpoint for {width}° width at {lat}° latitude"
                );
            }
        }
    }
}
