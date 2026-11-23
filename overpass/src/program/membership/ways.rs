use super::{remember, sparse_reads};
use crate::program::recurse::WayVertices;
use regol::{
    Bounds, Feature, FeatureType, Library, Point, PreparedQuery, Query, Selector, Types,
    source::Source,
};
use rstar::{AABB, RTree};
use std::{
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
    io,
    sync::LazyLock,
};

// Group nearby input nodes without scanning the gaps between distant clusters.
// 2^17 projected units span about 1.2 km at the equator.
const CELL_SHIFT: u32 = 17;

pub(super) fn prepare_parents<S: Source>(
    lib: &Library<S>,
) -> regol::Result<PreparedQuery<'_, 'static, S>> {
    static WAYS: LazyLock<Query> =
        LazyLock::new(|| Query::new(Selector::new(Types::WAY.union(Types::AREA))));
    WAYS.prepare(lib)
}

pub(super) fn parents<'a, 'q, S: Source>(
    query: &'q PreparedQuery<'a, '_, S>,
    input: &[Feature<'a, S>],
    retained_ids: bool,
    mut remaining: u64,
    check: &'q impl Fn() -> Result<(), String>,
) -> Result<impl Iterator<Item = regol::Result<Feature<'a, S>>> + 'q, String>
where
    'a: 'q,
{
    let mut sources = HashMap::new();
    let mut cells = BTreeMap::<_, Bounds>::new();
    let mut step = move || spend(&mut remaining, "parent way traversal steps", check);
    for feature in input {
        step().map_err(|e| e.to_string())?;
        if feature.kind() != FeatureType::Node {
            continue;
        }
        let Entry::Vacant(source) = sources.entry(feature.id().id) else {
            continue;
        };
        let point = sparse_reads(|| feature.point())
            .map_err(|e| e.to_string())?
            .ok_or("A node has no stored point.")?;
        source.insert(point);
        cells
            .entry((point.y >> CELL_SHIFT, point.x >> CELL_SHIFT))
            .and_modify(|bounds| {
                bounds.min.x = bounds.min.x.min(point.x);
                bounds.max.x = bounds.max.x.max(point.x);
                bounds.min.y = bounds.min.y.min(point.y);
                bounds.max.y = bounds.max.y.max(point.y);
            })
            .or_insert(Bounds {
                min: point,
                max: point,
            });
    }
    let points =
        retained_ids.then(|| RTree::bulk_load(sources.values().copied().map(xy).collect()));
    let mut cells = cells.into_values();
    let mut candidates = None;
    let mut seen = HashSet::new();
    let mut done = false;
    Ok(std::iter::from_fn(move || {
        if done {
            return None;
        }
        let result = (|| {
            loop {
                step()?;
                let candidate: Option<Feature<'_, S>> =
                    candidates.as_mut().and_then(Iterator::next).transpose()?;
                if let Some(way) = candidate {
                    // Area relations share the index with area ways. Verify each way
                    // against all input identities, even if several cells intersect it.
                    if way.kind() == FeatureType::Way
                        && remember(&mut seen, way.id())
                        && sparse_reads(|| {
                            contains_input(way, &sources, points.as_ref(), &mut step)
                        })?
                    {
                        return Ok(Some(way));
                    }
                    continue;
                }
                let Some(bounds) = cells.next() else {
                    return Ok(None);
                };
                candidates = Some(query.in_bounds(bounds));
            }
        })()
        .transpose();
        done = !matches!(result, Some(Ok(_)));
        result
    }))
}

fn xy(point: Point) -> [f64; 2] {
    [f64::from(point.x), f64::from(point.y)]
}

fn contains_input<S: Source>(
    way: Feature<'_, S>,
    sources: &HashMap<u64, Point>,
    points: Option<&RTree<[f64; 2]>>,
    step: &mut impl FnMut() -> regol::Result<()>,
) -> regol::Result<bool> {
    if let Some(points) = points {
        // The cell bounds can cover empty space. Skip decoding way vertices
        // when there is no actual input node within this way's bounds.
        let bounds = way.bounds()?;
        let envelope = AABB::from_corners(xy(bounds.min), xy(bounds.max));
        if points
            .locate_in_envelope_intersecting(&envelope)
            .next()
            .is_some()
        {
            for vertex in way.way_nodes()? {
                step()?;
                let vertex = vertex?;
                if vertex.id.and_then(|id| sources.get(&id)) == Some(&vertex.point) {
                    return Ok(true);
                }
            }
        }
    } else {
        for node in way.feature_nodes()? {
            step()?;
            if sources.contains_key(&node?.id().id) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn nodes<'a, 'q, S: Source>(
    input: &'q [Feature<'a, S>],
    mut remaining: u64,
    check: &'q impl Fn() -> Result<(), String>,
) -> impl Iterator<Item = regol::Result<Feature<'a, S>>> + 'q
where
    'a: 'q,
{
    let mut input = input.iter().copied();
    let mut vertices: Option<WayVertices<'a, S>> = None;
    let mut seen = HashSet::new();
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let result = sparse_reads(|| {
            loop {
                spend(&mut remaining, "way node traversal steps", check)?;
                if let Some(pending) = &mut vertices {
                    if let Some(node) = pending.next()? {
                        if remember(&mut seen, node.id()) {
                            return Ok(Some(node));
                        }
                        continue;
                    }
                    vertices = None;
                }
                let Some(owner) = input.next() else {
                    return Ok(None);
                };
                if owner.kind() == FeatureType::Way {
                    vertices = Some(WayVertices::new(owner, "Node membership (w)")?);
                }
            }
        })
        .transpose();
        done = !matches!(result, Some(Ok(_)));
        result
    })
}

fn spend(
    remaining: &mut u64,
    name: &'static str,
    check: &impl Fn() -> Result<(), String>,
) -> regol::Result<()> {
    check().map_err(io::Error::other)?;
    *remaining = remaining.checked_sub(1).ok_or(regol::Error::Limit(name))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn parent_way_scan_checks_budget_and_cancellation_after_grouping() {
        let lib =
            Library::new(include_bytes!("../../../tests/fixtures/membership-ids.gol").as_slice())
                .unwrap();
        let node = lib
            .features()
            .map(Result::unwrap)
            .find(|f| f.kind() == FeatureType::Node)
            .unwrap();
        let query = prepare_parents(&lib).unwrap();
        let mut limited = parents(&query, &[node], true, 3, &|| Ok(())).unwrap();
        assert!(matches!(
            limited.next(),
            Some(Err(regol::Error::Limit("parent way traversal steps")))
        ));
        assert!(limited.next().is_none());

        let checks = Cell::new(0);
        let check = || {
            checks.set(checks.get() + 1);
            if checks.get() == 4 {
                return Err("cancelled".into());
            }
            Ok(())
        };
        let mut cancelled = parents(&query, &[node], true, 100, &check).unwrap();
        assert_eq!(
            cancelled.next().unwrap().unwrap_err().to_string(),
            "cancelled"
        );
        assert!(cancelled.next().is_none());
    }
}
