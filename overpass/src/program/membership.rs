mod ways;

use crate::{MAX_INTERMEDIATE_FEATURES, overpass::Selection};
use regol::{
    Feature, FeatureId, FeatureLinks, FeatureType, Library, Members, PreparedQuery, source::Source,
};
use serde::Deserialize;
use std::{collections::HashSet, io, iter::from_fn};

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Direction {
    R,
    W,
    Bn,
    Bw,
    Br,
}

#[derive(Deserialize)]
pub(crate) struct Membership {
    direction: Direction,
    pub reference: String,
    role: Option<String>,
}

enum Links<'a, S> {
    Members(Members<'a, S>),
    Parents(FeatureLinks<'a, S>),
}

type Candidates<'a, 'q, S> = Box<dyn Iterator<Item = regol::Result<Feature<'a, S>>> + 'q>;

impl Membership {
    pub fn validate(&self, selection: &Selection) -> Result<(), String> {
        if matches!(self.direction, Direction::W) && !selection.is_node_only() {
            return Err("Use node(w) to select member nodes of ways.".into());
        }
        if matches!(self.direction, Direction::Bn) && selection.includes(FeatureType::Node) {
            return Err("Use way(bn), rel(bn) or wr(bn) to select parents of nodes.".into());
        }
        if matches!(self.direction, Direction::Bw | Direction::Br)
            && (!selection.includes(FeatureType::Relation)
                || selection.includes(FeatureType::Way)
                || selection.includes(FeatureType::Node))
        {
            return Err("Use rel(bw) or rel(br) to select parent relations.".into());
        }
        if self.role.is_some()
            && (matches!(self.direction, Direction::W) || self.selects_parent_ways(selection))
        {
            return Err(
                "Position and role restrictions on direct way and node membership are unsupported."
                    .into(),
            );
        }
        Ok(())
    }

    fn selects_parent_ways(&self, selection: &Selection) -> bool {
        matches!(self.direction, Direction::Bn) && selection.includes(FeatureType::Way)
    }

    pub fn prepare_parent_ways<'a, S: Source>(
        &self,
        lib: &'a Library<S>,
        selection: &Selection,
    ) -> regol::Result<Option<PreparedQuery<'a, 'static, S>>> {
        self.selects_parent_ways(selection)
            .then(|| ways::prepare_parents(lib))
            .transpose()
    }

    pub fn candidates<'a, 'q, S: Source>(
        &'q self,
        input: &'q [Feature<'a, S>],
        selection: &Selection,
        parent_ways: Option<&'q PreparedQuery<'a, '_, S>>,
        lib: &Library<S>,
        check: &'q impl Fn() -> Result<(), String>,
    ) -> Result<Candidates<'a, 'q, S>, String>
    where
        'a: 'q,
    {
        let remaining_steps = lib.limits().max_traversal_steps;
        if matches!(self.direction, Direction::W) {
            return Ok(Box::new(ways::nodes(input, remaining_steps, check)));
        }
        if let Some(query) = parent_ways {
            let parents =
                ways::parents(query, input, lib.has_way_node_ids(), remaining_steps, check)?;
            return if selection.includes(FeatureType::Relation) {
                Ok(Box::new(parents.chain(self.relations(input, check)?)))
            } else {
                Ok(Box::new(parents))
            };
        }
        Ok(Box::new(self.relations(input, check)?))
    }

    fn relations<'a, 'q, S: Source>(
        &'q self,
        input: &'q [Feature<'a, S>],
        check: &'q impl Fn() -> Result<(), String>,
    ) -> Result<impl Iterator<Item = regol::Result<Feature<'a, S>>> + 'q, String>
    where
        'a: 'q,
    {
        let source_kind = match self.direction {
            Direction::Bn => FeatureType::Node,
            Direction::Bw => FeatureType::Way,
            Direction::R | Direction::Br => FeatureType::Relation,
            Direction::W => unreachable!(),
        };
        let backwards = !matches!(self.direction, Direction::R);
        let role = self.role.as_deref();
        let mut input = input
            .iter()
            .copied()
            .filter(move |f| f.kind() == source_kind);
        // An input set may link to the same parent through many children. Scan
        // that parent's roles once against all input identities.
        let mut sources = HashSet::new();
        if backwards && role.is_some() {
            for source in input.clone() {
                check()?;
                sources.insert(source.id());
            }
        }
        let mut visited = HashSet::new();
        let mut links = None;
        let mut done = false;
        Ok(from_fn(move || {
            if done {
                return None;
            }
            let result = sparse_reads(|| {
                loop {
                    check().map_err(io::Error::other)?;
                    match links.as_mut() {
                        Some(Links::Members(members)) => {
                            if let Some(member) = members.next().transpose()? {
                                if role.is_none_or(|r| r == member.role)
                                    && remember(&mut visited, member.feature.id())
                                {
                                    return Ok(Some(member.feature));
                                }
                                continue;
                            }
                        }
                        Some(Links::Parents(parents)) => {
                            if let Some(parent) = parents.next().transpose()? {
                                if remember(&mut visited, parent.id())
                                    && match role {
                                        None => true,
                                        Some(role) => has_role(parent, &sources, role, check)?,
                                    }
                                {
                                    return Ok(Some(parent));
                                }
                                continue;
                            }
                        }
                        None => {}
                    }
                    let Some(owner) = input.next() else {
                        return Ok(None);
                    };
                    links = Some(if backwards {
                        Links::Parents(owner.parent_relations()?)
                    } else {
                        Links::Members(owner.members()?)
                    });
                }
            })
            .transpose();
            done = !matches!(result, Some(Ok(_)));
            result
        }))
    }
}

pub(super) fn remember(ids: &mut HashSet<FeatureId>, id: FeatureId) -> bool {
    if ids.len() < MAX_INTERMEDIATE_FEATURES {
        return ids.insert(id);
    }
    // Cap the traversal cache. The result collector still deduplicates matches.
    !ids.contains(&id)
}

fn has_role<S: Source>(
    parent: Feature<'_, S>,
    sources: &HashSet<FeatureId>,
    role: &str,
    check: &impl Fn() -> Result<(), String>,
) -> regol::Result<bool> {
    for member in parent.members()? {
        check().map_err(io::Error::other)?;
        let member = member?;
        if member.role == role && sources.contains(&member.feature.id()) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn sparse_reads<T>(read: impl FnOnce() -> T) -> T {
    // Following links can touch tiles outside the indexed scan windows.
    #[cfg(target_arch = "wasm32")]
    return crate::read_hints::with_relation_reads(read);
    #[cfg(not(target_arch = "wasm32"))]
    read()
}

#[cfg(test)]
mod tests {
    use crate::{GroupData, execute_groups};
    use regol::Library;
    use serde_json::{Value, json};

    const STRESS: &[u8] = include_bytes!("../../tests/fixtures/stress.gol");
    const MONACO: &[u8] = include_bytes!("../../tests/fixtures/monaco-2026-01-01.gol");
    const WAY_LINKS: &[u8] = include_bytes!("../../tests/fixtures/membership.gol");
    const WAY_IDS: &[u8] = include_bytes!("../../tests/fixtures/membership-ids.gol");

    fn ids(lib: &Library<&[u8]>, windows: Value, sets: Value, show: &str) -> Vec<String> {
        let request = json!({"windows":windows,"sets":sets,"show":[show],"output":"center"});
        let mut groups =
            execute_groups(lib, serde_json::from_value(request).unwrap(), &|| Ok(())).unwrap();
        assert_eq!(groups.len(), 1);
        let GroupData::Features(features) = groups.remove(0).data else {
            panic!("expected selected features");
        };
        let mut ids: Vec<_> = features
            .into_iter()
            .map(|feature| feature.id().to_string())
            .collect();
        ids.sort();
        ids
    }

    fn membership(name: &str, kind: &str, direction: &str, reference: &str, role: Value) -> Value {
        json!({
            "name":name,"source":"","overpass":{"kind":kind,"tags":[],"windows":[]},
            "membership":{"direction":direction,"reference":reference,"role":role}
        })
    }

    #[test]
    fn linked_members_and_parents_extend_past_source_bbox_and_match_exact_roles() {
        let lib = Library::new(STRESS).unwrap();
        let windows = json!([[-0.00161, 50.99999, -0.00159, 51.00001]]);
        let mut clipped = membership("clipped", "way", "r", "route", json!("forward"));
        clipped["overpass"]["windows"] = windows.clone();
        clipped["overpass"]["bbox"] = json!(true);
        let sets = json!([
            {"name":"stop","source":"n[name=\"Grid 1\"]"},
            membership("route", "relation", "bn", "stop", json!("stop")),
            membership("no_route", "relation", "bn", "stop", json!("")),
            membership("stops", "node", "r", "route", json!("stop")),
            membership("empty_role", "way", "r", "route", json!("")),
            membership("forward", "way", "r", "route", json!("forward")),
            membership("road_route", "relation", "bw", "empty_role", json!("")),
            {"name":"all_ways","source":"","union":["forward","empty_role"]},
            {"name":"round_trip","source":"","difference":["all_ways","empty_role"]},
            {"name":"copied","source":"$round_trip","overpass":{"kind":"way","tags":[],"windows":[]}},
            clipped
        ]);
        let cases: [(&str, &[&str]); 7] = [
            ("route", &["R3000"]),
            ("no_route", &[]),
            ("stops", &["N1"]),
            ("empty_role", &["W2000"]),
            ("road_route", &["R3000"]),
            ("copied", &["W2001"]),
            ("clipped", &[]),
        ];
        for (name, expected) in cases {
            assert_eq!(ids(&lib, windows.clone(), sets.clone(), name), expected);
        }
    }

    #[test]
    fn relation_to_relation_membership_follows_one_step_at_a_time() {
        let lib = Library::new(MONACO).unwrap();
        let windows = json!([[7.407, 43.72, 7.44, 43.75]]);
        let sets = json!([
            {"name":"country","source":"a[boundary=administrative][admin_level=2][name=Monaco]"},
            membership("city", "relation", "r", "country", json!("subarea")),
            membership("city_parent", "relation", "br", "city", json!("subarea"))
        ]);
        assert_eq!(
            ids(&lib, windows.clone(), sets.clone(), "city"),
            ["R2220322"]
        );
        assert_eq!(ids(&lib, windows, sets, "city_parent"), ["R1124039"]);
    }

    #[test]
    fn way_and_node_membership_deduplicates_by_identity_with_or_without_original_ids() {
        let windows = json!([[0.00019, 0.00019, 0.00041, 0.00041]]);
        let mut clipped = membership("clipped", "node", "w", "owners", Value::Null);
        clipped["overpass"]["windows"] = windows.clone();
        clipped["overpass"]["bbox"] = json!(true);
        let sets = json!([
            {"name":"main","source":"w[name=Main]"},
            {"name":"ring","source":"a[name=Ring]"},
            {"name":"first","source":"n[name=First]"},
            {"name":"coincident","source":"n[name=Coincident]"},
            {"name":"owners","source":"","union":["main","ring","first"]},
            membership("children", "node", "w", "owners", Value::Null),
            membership("parents", "way", "bn", "first", Value::Null),
            membership("coincident_parents", "way", "bn", "coincident", Value::Null),
            membership("all_parents", "way", "bn", "children", Value::Null),
            membership("both_parents", "wr", "bn", "first", Value::Null),
            clipped
        ]);
        let cases: [(&str, &[&str]); 6] = [
            ("children", &["N1", "N3", "N4", "N5", "N6"]),
            ("parents", &["W10", "W12", "W13"]),
            ("coincident_parents", &["W11"]),
            ("all_parents", &["W10", "W11", "W12", "W13"]),
            ("both_parents", &["R20", "W10", "W12", "W13"]),
            ("clipped", &["N1", "N3"]),
        ];
        for (data, retained_ids) in [(WAY_LINKS, false), (WAY_IDS, true)] {
            let lib = Library::new(data).unwrap();
            assert_eq!(lib.has_way_node_ids(), retained_ids);
            for (name, expected) in cases {
                assert_eq!(
                    ids(&lib, windows.clone(), sets.clone(), name),
                    expected,
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn way_membership_reports_missing_feature_records() {
        for (data, diagnostic) in [
            (
                WAY_LINKS,
                "requires a feature record for every vertex of way 13",
            ),
            (
                WAY_IDS,
                "requires node 7 from way 13 to have a feature record",
            ),
        ] {
            let lib = Library::new(data).unwrap();
            let request = json!({
                "windows": [[0.00019, 0.00019, 0.00041, 0.00041]],
                "sets": [
                    {"name":"way","source":"w[name=\"Geometry-only vertex\"]"},
                    membership("children", "node", "w", "way", Value::Null),
                ],
                "show": ["children"], "output": "center"
            });
            let error = execute_groups(&lib, serde_json::from_value(request).unwrap(), &|| Ok(()))
                .err()
                .unwrap();
            assert!(error.contains(diagnostic), "{error}");
        }
    }

    #[test]
    fn direct_membership_validates_result_kinds_and_restrictions() {
        let lib = Library::new(WAY_IDS).unwrap();
        for (kind, direction, role, diagnostic) in [
            ("way", "w", Value::Null, "member nodes of ways"),
            ("node", "bn", Value::Null, "parents of nodes"),
            ("way", "bw", Value::Null, "parent relations"),
            ("wr", "br", Value::Null, "parent relations"),
            ("node", "w", json!("1"), "membership are unsupported"),
            ("way", "bn", json!("-1"), "membership are unsupported"),
            ("wr", "bn", json!("stop"), "membership are unsupported"),
        ] {
            let request = json!({
                "windows": [[0.00019, 0.00019, 0.00041, 0.00041]],
                "sets": [
                    {"name":"source","source":"*"},
                    membership("result", kind, direction, "source", role),
                ],
                "show": ["result"], "output": "center"
            });
            let error = execute_groups(&lib, serde_json::from_value(request).unwrap(), &|| Ok(()))
                .err()
                .unwrap();
            assert!(error.contains(diagnostic), "{error}");
        }
    }

    #[test]
    fn difference_and_three_way_intersection_preserve_feature_identity() {
        let lib = Library::new(STRESS).unwrap();
        let windows = json!([[-0.00161, 50.99999, -0.00139, 51.00001]]);
        let sets = json!([
            {"name":"all","source":"n[capacity]"},
            {"name":"exclude","source":"n[amenity=no]"},
            {"name":"diff","source":"","difference":["all","exclude"]},
            {"name":"school","source":"n[amenity=school]"},
            {"name":"result","source":"$all","intersect":["diff","school"],"overpass":{"kind":"node","tags":[],"windows":[]}}
        ]);
        assert_eq!(
            ids(&lib, windows.clone(), sets.clone(), "diff"),
            ["N1", "N3"]
        );
        assert_eq!(ids(&lib, windows, sets, "result"), ["N3"]);
    }
}
