use super::membership::{remember, sparse_reads};
use regol::{Feature, FeatureLinks, FeatureType, Members, WayNodes, source::Source};
use std::{collections::HashSet, io, iter::Peekable};

pub(super) fn candidates<'a, 'q, S: Source>(
    input: &'q [Feature<'a, S>],
    mut remaining_steps: u64,
    check: &'q impl Fn() -> Result<(), String>,
) -> impl Iterator<Item = regol::Result<Feature<'a, S>>> + 'q
where
    'a: 'q,
{
    let mut input = input.iter().copied();
    let mut members: Option<Members<'a, S>> = None;
    let mut vertices: Option<WayVertices<'a, S>> = None;
    let mut expanded_ways = HashSet::new();
    let mut done = false;
    let mut spend_step = move || -> regol::Result<()> {
        remaining_steps = remaining_steps
            .checked_sub(1)
            .ok_or(regol::Error::Limit("downward recurse traversal steps"))?;
        Ok(())
    };
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let result = sparse_reads(|| {
            loop {
                check().map_err(io::Error::other)?;
                if let Some(pending) = &mut vertices {
                    if let Some(node) = pending.next()? {
                        spend_step()?;
                        return Ok(Some(node));
                    }
                    vertices = None;
                }
                if let Some(pending) = &mut members {
                    if let Some(member) = pending.next().transpose()? {
                        spend_step()?;
                        let feature = member.feature;
                        match feature.kind() {
                            FeatureType::Node => return Ok(Some(feature)),
                            FeatureType::Way => {
                                if remember(&mut expanded_ways, feature.id()) {
                                    vertices = Some(WayVertices::new(feature, "Recurse down (>)")?);
                                }
                                return Ok(Some(feature));
                            }
                            FeatureType::Relation => continue,
                        }
                    }
                    members = None;
                }
                let Some(owner) = input.next() else {
                    return Ok(None);
                };
                spend_step()?;
                match owner.kind() {
                    FeatureType::Node => {}
                    FeatureType::Way => {
                        if remember(&mut expanded_ways, owner.id()) {
                            vertices = Some(WayVertices::new(owner, "Recurse down (>)")?);
                        }
                    }
                    FeatureType::Relation => {
                        // The producer records an omission count without member types.
                        if owner
                            .tag("geodesk:missing_members")?
                            .and_then(|tag| tag.as_number())
                            .is_some_and(|count| count > 0.0)
                        {
                            return Err(io::Error::other(format!(
                                "Recurse down (>) requires complete membership for relation {} in this GOL.",
                                owner.id().id
                            ))
                            .into());
                        }
                        members = Some(owner.members()?);
                    }
                }
            }
        })
        .transpose();
        done = !matches!(result, Some(Ok(_)));
        result
    })
}

pub(super) struct WayVertices<'a, S: Source> {
    way: Feature<'a, S>,
    vertices: Peekable<WayNodes<'a, S>>,
    records: FeatureLinks<'a, S>,
    operation: &'static str,
    started: bool,
}

impl<'a, S: Source> WayVertices<'a, S> {
    pub(super) fn new(way: Feature<'a, S>, operation: &'static str) -> regol::Result<Self> {
        Ok(Self {
            way,
            vertices: way.way_nodes()?.peekable(),
            records: way.feature_nodes()?,
            operation,
            started: false,
        })
    }

    pub(super) fn next(&mut self) -> regol::Result<Option<Feature<'a, S>>> {
        let vertex = self.vertices.next().transpose()?;
        let record = self.records.next().transpose()?;
        let Some(vertex) = vertex else {
            return if record.is_none() {
                Ok(None)
            } else {
                Err(io::Error::other(format!(
                    "Way {} has more node records than vertices.",
                    self.way.id().id
                ))
                .into())
            };
        };
        if let Some(record) = record {
            if record.point()? == Some(vertex.point)
                && vertex.id.is_none_or(|id| id == record.id().id)
            {
                self.started = true;
                return Ok(Some(record));
            }
        } else if self.started && self.way.is_area() && self.vertices.peek().is_none() {
            // The reader repeats an area's first vertex to close the ring.
            return Ok(None);
        }
        let way = self.way.id().id;
        let operation = self.operation;
        let message = match vertex.id {
            Some(id) => format!(
                "{operation} requires node {id} from way {way} to have a feature record in this GOL."
            ),
            None => format!(
                "{operation} requires a feature record for every vertex of way {way} in this GOL."
            ),
        };
        Err(io::Error::other(message).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regol::Library;
    use std::cell::Cell;

    #[test]
    fn empty_results_can_still_exhaust_traversal_budget_or_be_cancelled() {
        let data = include_bytes!("../../tests/fixtures/stress.gol");
        let lib = Library::new(data.as_slice()).unwrap();
        let node = lib
            .features()
            .map(Result::unwrap)
            .find(|feature| feature.kind() == FeatureType::Node)
            .unwrap();
        let input = [node; 3];
        let mut limited = candidates(&input, 2, &|| Ok(()));
        assert!(matches!(limited.next(), Some(Err(regol::Error::Limit(_)))));
        assert!(limited.next().is_none());
        let checks = Cell::new(0);
        let check = || {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                return Err("cancelled".to_string());
            }
            Ok(())
        };
        let mut cancelled = candidates(&input, 10, &check);
        assert_eq!(
            cancelled.next().unwrap().unwrap_err().to_string(),
            "cancelled"
        );
        assert!(cancelled.next().is_none());
    }
}
