use super::*;
use crate::execute;
use regol::Library;
use serde_json::{Value, json};

const STRESS: &[u8] = include_bytes!("../../../tests/fixtures/stress.gol");
const WINDOW: [f64; 4] = [-0.001601, 51.000099, -0.001599, 51.000101];

fn query(kind: &str, name: &str, space: Value) -> Value {
    let mut overpass = json!({
        "kind": kind, "tags": [{"key": "name", "op": "eq", "value": name}],
        "bbox": false,
    });
    overpass
        .as_object_mut()
        .unwrap()
        .extend(space.as_object().unwrap().clone());
    json!({"name": "selected", "source": "*", "overpass": overpass})
}

fn count(sets: Vec<Value>, show: &str) -> usize {
    let lib = Library::new(STRESS).unwrap();
    let plan = json!({
        "windows": [WINDOW], "sets": sets, "show": [show], "output": "count",
    });
    execute(&lib, serde_json::from_value(plan).unwrap())
        .unwrap()
        .counts
        .unwrap()
        .total
}

fn reference() -> Value {
    let mut set = query("way", "Area 0", json!({"bbox": true, "windows": [WINDOW]}));
    set["name"] = json!("outline");
    set
}

#[test]
fn poly_tests_the_way_path_including_crossing_segments() {
    let interior = json!({"polygon": [
        [-0.00156, 51.00004], [-0.00154, 51.00004],
        [-0.00154, 51.00006], [-0.00156, 51.00006],
    ]});
    let crossing = json!({"polygon": [
        [-0.00156, 50.99999], [-0.00154, 50.99999],
        [-0.00154, 51.00001], [-0.00156, 51.00001],
    ]});
    assert_eq!(count(vec![query("way", "Area 0", interior)], "selected"), 0);
    assert_eq!(count(vec![query("way", "Area 0", crossing)], "selected"), 1);
}

#[test]
fn around_uses_complete_way_outlines_for_references_and_targets() {
    let around = |point| json!({"around": {"point": point, "meters": 1}});
    let center = [-0.00155, 51.00005];
    let edge = [-0.00155, 51.0];
    assert_eq!(
        count(vec![query("way", "Area 0", around(center))], "selected"),
        0
    );
    assert_eq!(
        count(vec![query("way", "Area 0", around(edge))], "selected"),
        1
    );
    let proximity = json!({"around": {"reference": "outline", "meters": 1}});
    assert_eq!(
        count(
            vec![reference(), query("node", "Grid 2", proximity.clone())],
            "selected"
        ),
        1
    );
    assert_eq!(
        count(
            vec![reference(), query("way", "Path 1", proximity)],
            "selected"
        ),
        1
    );
}

#[test]
fn area_membership_includes_border_nodes_and_requires_way_interior() {
    let mapped = json!({"name": "areas", "source": "$outline", "map_to_area": true});
    let area = json!({"area": "areas"});
    assert_eq!(
        count(
            vec![
                reference(),
                mapped.clone(),
                query("node", "Grid 2", area.clone())
            ],
            "selected"
        ),
        1
    );
    assert_eq!(
        count(
            vec![reference(), mapped, query("way", "Path 1", area)],
            "selected"
        ),
        0
    );
}

fn matches(area: &Areas, points: &[Point], include_boundary: bool) -> bool {
    area.matches_path(points, include_boundary, &|| Ok(()))
        .unwrap()
}

fn project(point: [f64; 2]) -> Point {
    geometry::geographic_point(point).unwrap().project()
}

fn rectangle(west: f64, south: f64, east: f64, north: f64) -> Vec<Point> {
    [
        [west, south],
        [east, south],
        [east, north],
        [west, north],
        [west, south],
    ]
    .map(project)
    .into()
}

#[test]
fn filled_areas_keep_holes_and_crossing_lines() {
    let area = Areas::new(
        prepare_areas(vec![Polygon {
            exterior: rectangle(0.0, 0.0, 10.0, 10.0),
            interiors: vec![rectangle(4.0, 4.0, 6.0, 6.0)],
        }])
        .unwrap(),
        false,
    );
    assert!(!matches(&area, &[project([5.0, 5.0])], true));
    assert!(matches(&area, &[project([4.0, 5.0])], true));
    assert!(!matches(
        &area,
        &[[4.5, 4.0], [5.5, 4.0]].map(project),
        false
    ));
    assert!(matches(
        &area,
        &[[-1.0, 5.0], [11.0, 5.0]].map(project),
        false
    ));
}

#[test]
fn wrapped_literal_bounds_and_candidates_follow_the_antimeridian() {
    let greenwich = Areas::polygon(&[[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]]).unwrap();
    let area =
        Areas::polygon(&[[179.5, -1.0], [-179.5, -1.0], [-179.5, 1.0], [179.5, 1.0]]).unwrap();
    let windows = area.windows(&|| Ok(())).unwrap();
    assert_eq!(windows.len(), 2);
    assert!(
        windows
            .iter()
            .any(|w| w.min.x == i32::MIN && w.max.x == project([-179.5, 0.0]).x)
    );
    assert!(
        windows
            .iter()
            .any(|w| w.max.x == i32::MAX && w.min.x == project([179.5, 0.0]).x)
    );
    for lon in [-180.0, -179.75, 179.75, 180.0] {
        assert!(matches(&area, &[project([lon, 0.0])], true));
    }
    assert!(!matches(&area, &[project([0.0, 0.0])], true));
    let crossing = [[-179.0, 0.0], [179.0, 0.0]].map(project);
    assert!(!matches(&greenwich, &crossing, true));
    assert!(matches(&area, &crossing, false));
    assert!(matches(
        &area,
        &[[179.5, 0.0], [179.5, 0.5]].map(project),
        true
    ));
    assert!(!matches(
        &area,
        &[[179.5, 0.0], [179.5, 0.5]].map(project),
        false
    ));
}

#[test]
fn wrapped_mapped_shells_reassign_holes_and_preserve_seam_topology() {
    let area = Areas::new(
        prepare_areas(vec![
            Polygon {
                exterior: rectangle(-2.0, -2.0, 2.0, 2.0),
                // Flat assembly can attach a wrapped hole to the first component.
                interiors: vec![rectangle(-179.5, -1.0, 179.5, 1.0)],
            },
            Polygon {
                exterior: rectangle(178.0, -2.0, -178.0, 2.0),
                interiors: vec![],
            },
        ])
        .unwrap(),
        false,
    );
    assert!(matches(&area, &[project([0.0, 0.0])], true));
    assert!(matches(&area, &[project([179.0, 0.0])], true));
    assert!(!matches(&area, &[project([179.75, 0.0])], true));
    assert!(matches(&area, &[project([179.5, 0.0])], true));
    assert!(matches(
        &area,
        &[[180.0, 1.25], [-180.0, 1.75]].map(project),
        false
    ));
    assert!(!matches(
        &area,
        &[[180.0, -0.5], [-180.0, 0.5]].map(project),
        false
    ));
    let hole_border = [[179.75, -1.0], [-179.75, -1.0]].map(project);
    assert!(matches(&area, &hole_border, true));
    assert!(!matches(&area, &hole_border, false));
}

#[test]
fn long_candidate_outside_the_local_frame_keeps_its_exact_segments() {
    let area =
        Areas::polygon(&[[-179.5, -1.0], [179.5, -1.0], [179.5, 1.0], [-179.5, 1.0]]).unwrap();
    let path = [[-179.0, 0.0], [-1.0, 0.0], [1.0, 0.0], [179.75, 0.0]].map(project);
    assert!(matches(&area, &path, false));
    let misses = [[-179.0, 0.0], [-1.0, 0.0], [1.0, 0.0], [179.0, 0.0]].map(project);
    assert!(!matches(&area, &misses, true));
}

#[test]
fn once_winding_rings_select_the_smaller_polar_region_in_either_direction() {
    for latitude in [-75.0_f64, 75.0] {
        let mut vertices = [[-120.0, latitude], [0.0, latitude], [120.0, latitude]];
        for _ in 0..2 {
            let area = Areas::polygon(&vertices).unwrap();
            let windows = area.windows(&|| Ok(())).unwrap();
            assert_eq!(windows.len(), 1);
            assert_eq!(windows[0].min.x, i32::MIN);
            assert_eq!(windows[0].max.x, i32::MAX);
            for lon in [-180.0, -120.0, 0.0, 179.0, 180.0] {
                assert!(matches(
                    &area,
                    &[project([lon, latitude.signum() * 90.0])],
                    false
                ));
                assert!(!matches(
                    &area,
                    &[project([lon, latitude.signum() * 70.0])],
                    true
                ));
            }
            let path = [[179.0, latitude], [-179.0, latitude]].map(project);
            assert!(matches(&area, &path, true));
            assert!(!matches(&area, &path, false));
            vertices.reverse();
        }
    }
    assert!(Areas::polygon(&[[-120.0, 0.0], [0.0, 0.0], [120.0, 0.0]]).is_err());
}

#[test]
fn wrapped_holes_in_polar_areas_stay_holes_on_both_sides() {
    let area = Areas::new(
        prepare_areas(vec![
            Polygon {
                exterior: rectangle(-1.0, -1.0, 1.0, 1.0),
                interiors: vec![rectangle(178.0, 79.0, -178.0, 80.0)],
            },
            Polygon {
                exterior: [[-120.0, 75.0], [0.0, 75.0], [120.0, 75.0], [-120.0, 75.0]]
                    .map(project)
                    .into(),
                interiors: Vec::new(),
            },
        ])
        .unwrap(),
        false,
    );
    for longitude in [-180.0, -179.0, 179.0, 180.0] {
        assert!(!matches(&area, &[project([longitude, 79.5])], true));
        assert!(matches(&area, &[project([longitude, 79.0])], true));
        assert!(matches(&area, &[project([longitude, 81.0])], false));
    }
}

#[test]
fn a_winding_ring_can_backtrack_across_its_polar_cut() {
    let area = Areas::polygon(&[
        [0.0, 75.0],
        [100.0, 74.0],
        [50.0, 78.0],
        [150.0, 75.0],
        [-110.0, 76.0],
        [-10.0, 75.0],
    ])
    .unwrap();
    for longitude in [-180.0, -100.0, 0.0, 50.0, 100.0, 180.0] {
        assert!(matches(&area, &[project([longitude, 81.0])], false));
        assert!(!matches(&area, &[project([longitude, 71.0])], true));
    }
}

#[test]
fn polar_bands_exclude_the_inner_cap_and_keep_both_true_boundaries() {
    for sign in [-1.0, 1.0] {
        let ring = |latitude: f64| {
            [-120.0, 0.0, 120.0, -120.0]
                .map(|x| project([x, sign * latitude]))
                .to_vec()
        };
        for reverse in [false, true] {
            let mut exterior = ring(72.0);
            let mut interior = ring(80.0);
            if reverse {
                exterior.reverse();
            } else {
                interior.reverse();
            }
            let area = Areas::new(
                prepare_areas(vec![
                    Polygon {
                        exterior: rectangle(-2.0, -2.0, 2.0, 2.0),
                        interiors: vec![interior],
                    },
                    Polygon {
                        exterior,
                        interiors: vec![],
                    },
                ])
                .unwrap(),
                false,
            );
            for longitude in [-180.0, -120.0, 0.0, 179.0, 180.0] {
                for latitude in [70.0, 81.0, 90.0] {
                    assert!(!matches(
                        &area,
                        &[project([longitude, sign * latitude])],
                        true
                    ));
                }
                assert!(matches(&area, &[project([longitude, sign * 75.0])], false));
                for latitude in [72.0, 80.0] {
                    let border = project([longitude, sign * latitude]);
                    assert!(matches(&area, &[border], true));
                    assert!(!matches(&area, &[border], false));
                }
            }
            let crossing = [[179.0, sign * 73.0], [-179.0, sign * 81.0]].map(project);
            assert!(matches(&area, &crossing, false));
            let border = [[179.0, sign * 80.0], [-179.0, sign * 80.0]].map(project);
            assert!(matches(&area, &border, true));
            assert!(!matches(&area, &border, false));
        }
    }
}

#[test]
fn polar_band_joins_avoid_backtracking_boundary_edges() {
    let ring = |offset: f64| {
        [
            [0.0, 72.0],
            [100.0, 71.0],
            [50.0, 74.0],
            [150.0, 72.0],
            [-110.0, 73.0],
            [-10.0, 72.0],
            [0.0, 72.0],
        ]
        .map(|[x, y]| project([x, y + offset]))
        .to_vec()
    };
    let area = Areas::new(
        prepare_areas(vec![Polygon {
            exterior: ring(0.0),
            interiors: vec![ring(8.0)],
        }])
        .unwrap(),
        false,
    );
    assert!(matches(&area, &[project([50.0, 76.0])], false));
    assert!(!matches(&area, &[project([50.0, 84.0])], true));
}

#[test]
fn a_contractible_polygon_can_cover_all_longitudes_at_different_latitudes() {
    let area = Areas::polygon(&[
        [-170.0, 0.0],
        [-50.0, 10.0],
        [80.0, 20.0],
        [-160.0, 30.0],
        [-140.0, 40.0],
        [-140.0, 42.0],
        [-160.0, 32.0],
        [80.0, 22.0],
        [-50.0, 12.0],
        [-170.0, 2.0],
    ])
    .unwrap();
    assert!(matches(&area, &[project([-160.0, 1.5])], false));
    assert!(matches(&area, &[project([-160.0, 31.0])], false));
    assert!(!matches(&area, &[project([-160.0, 15.0])], true));
    let windows = area.windows(&|| Ok(())).unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].min.x, i32::MIN);
    assert_eq!(windows[0].max.x, i32::MAX);
}

#[test]
fn a_winding_hole_can_touch_its_shell_at_one_point() {
    for sign in [-1.0, 1.0] {
        let position = |[x, y]: [f64; 2]| project([x, sign * y]);
        let exterior = [[-120.0, 72.0], [0.0, 72.0], [120.0, 72.0], [-120.0, 72.0]]
            .map(position)
            .to_vec();
        let interior = [
            [-120.0, 80.0],
            [0.0, 80.0],
            [60.0, 72.0],
            [120.0, 80.0],
            [-120.0, 80.0],
        ]
        .map(position)
        .to_vec();
        let area = Areas::new(
            prepare_areas(vec![Polygon {
                exterior,
                interiors: vec![interior, rectangle(-91.0, sign * 75.0, -89.0, sign * 76.0)],
            }])
            .unwrap(),
            false,
        );
        let touch = position([60.0, 72.0]);
        assert!(matches(&area, &[touch], true));
        assert!(!matches(&area, &[touch], false));
        assert!(!matches(&area, &[position([60.0, 73.0])], true));
        assert!(matches(&area, &[position([0.0, 75.0])], false));
        assert!(matches(&area, &[position([179.0, 75.0])], false));
        assert!(!matches(&area, &[position([-90.0, 75.5])], true));
        let border = [[59.0, 72.0], [61.0, 72.0]].map(position);
        assert!(matches(&area, &border, true));
        assert!(!matches(&area, &border, false));
    }
}

#[test]
fn a_polar_band_rejects_multiple_pinches_crossings_and_shared_edges() {
    let exterior = [[-120.0, 72.0], [0.0, 72.0], [120.0, 72.0], [-120.0, 72.0]]
        .map(project)
        .to_vec();
    for inner in [
        vec![
            [0.0, 80.0],
            [30.0, 72.0],
            [45.0, 80.0],
            [60.0, 72.0],
            [120.0, 80.0],
            [-120.0, 80.0],
            [0.0, 80.0],
        ],
        vec![
            [0.0, 80.0],
            [30.0, 72.0],
            [60.0, 72.0],
            [120.0, 80.0],
            [-120.0, 80.0],
            [0.0, 80.0],
        ],
        vec![
            [0.0, 80.0],
            [60.0, 71.0],
            [120.0, 80.0],
            [-120.0, 80.0],
            [0.0, 80.0],
        ],
    ] {
        assert!(
            prepare_areas(vec![Polygon {
                exterior: exterior.clone(),
                interiors: vec![inner.into_iter().map(project).collect()],
            }])
            .is_err()
        );
    }
}
