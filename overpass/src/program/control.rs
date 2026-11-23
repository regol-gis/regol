use crate::{
    MAX_INTERMEDIATE_FEATURES,
    overpass::{NodePosition, NodeRadius},
};
use regol::{Feature, FeatureId, FeatureType, source::Source};
use rstar::{RTree, primitives::GeomWithData};
use serde::Deserialize;
use std::{
    cell::OnceCell,
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
};

const MAX_DEPTH: usize = 16;
const MAX_STATEMENTS: usize = 256;
const MAX_COMPARISONS: usize = 256;
const MAX_NAMES: usize = 64;
const MAX_IMPORTS: usize = 256;
const MAX_WORK: usize = 20_000_000;
const MEMORY_LIMIT: &str =
    "Foreach exceeds the intermediate feature budget. Narrow the input sets.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Loop {
    source: String,
    item: String,
    inputs: BTreeMap<String, String>,
    accumulator: String,
    initial: Option<String>,
    body: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Nearby {
        source: String,
        reference: String,
        meters: f64,
        target: String,
    },
    Copy {
        source: String,
        target: String,
    },
    Union {
        members: Vec<String>,
        target: String,
    },
    If {
        comparisons: Vec<Comparison>,
        body: Vec<Step>,
    },
    Accumulate {
        members: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Comparison {
    source: String,
    op: Operator,
    count: u32,
}

#[derive(Deserialize)]
enum Operator {
    #[serde(rename = "==")]
    Equal,
    #[serde(rename = "!=")]
    NotEqual,
    #[serde(rename = "<")]
    Less,
    #[serde(rename = "<=")]
    AtMost,
    #[serde(rename = ">")]
    Greater,
    #[serde(rename = ">=")]
    AtLeast,
}

impl Comparison {
    fn matches(&self, count: usize) -> bool {
        let count = count as u32;
        match self.op {
            Operator::Equal => count == self.count,
            Operator::NotEqual => count != self.count,
            Operator::Less => count < self.count,
            Operator::AtMost => count <= self.count,
            Operator::Greater => count > self.count,
            Operator::AtLeast => count >= self.count,
        }
    }
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
        && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

#[derive(Default)]
struct Validation<'a> {
    statements: usize,
    comparisons: usize,
    accumulated: bool,
    locals: HashSet<&'a str>,
}

impl Loop {
    pub(crate) fn inputs(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.source.as_str())
            .chain(self.initial.as_deref())
            .chain(self.inputs.values().map(String::as_str))
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.inputs.len() > MAX_IMPORTS
            || self.item == self.accumulator
            || !valid_name(&self.item)
            || !valid_name(&self.accumulator)
            || self.item == "_"
            || self.accumulator == "_"
            || self.inputs.contains_key(&self.item)
            || self.inputs.contains_key(&self.accumulator)
            || self.inputs.keys().any(|name| !valid_name(name))
        {
            return Err("Invalid foreach item, input or accumulator names.".into());
        }
        let mut validation = Validation::default();
        validation.locals.insert(self.item.as_str());
        let mut bindings = self
            .inputs
            .keys()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        bindings.insert(&self.item);
        self.validate_steps(&self.body, &mut bindings, &mut validation, 0)?;
        if !validation.accumulated {
            return Err("A foreach loop needs an accumulating set union.".into());
        }
        Ok(())
    }

    fn validate_steps<'a>(
        &'a self,
        steps: &'a [Step],
        bindings: &mut HashSet<&'a str>,
        validation: &mut Validation<'a>,
        depth: usize,
    ) -> Result<(), String> {
        if depth >= MAX_DEPTH {
            return Err("Foreach block nesting limit reached.".into());
        }
        for step in steps {
            validation.statements += 1;
            if validation.statements > MAX_STATEMENTS {
                return Err("Foreach statement limit reached.".into());
            }
            let read = |alias: &str| {
                if bindings.contains(alias) {
                    Ok(())
                } else {
                    Err(format!("Undefined set .{alias} in foreach."))
                }
            };
            let verify_members = |members: &[String]| -> Result<(), String> {
                if members.len() > MAX_NAMES {
                    return Err("A foreach union can use up to 64 named sets.".into());
                }
                for member in members {
                    read(member)?;
                }
                Ok(())
            };
            let target = match step {
                Step::Nearby {
                    source,
                    reference,
                    meters,
                    target,
                } => {
                    read(source)?;
                    read(reference)?;
                    if !meters.is_finite() || *meters < 0.0 {
                        return Err("Radius must be finite and nonnegative.".into());
                    }
                    target
                }
                Step::Copy { source, target } => {
                    read(source)?;
                    target
                }
                Step::Union { members, target } => {
                    if members.is_empty() {
                        return Err("A union needs at least one set.".into());
                    }
                    verify_members(members)?;
                    target
                }
                Step::If { comparisons, body } => {
                    validation.comparisons += comparisons.len();
                    if comparisons.is_empty() || validation.comparisons > MAX_COMPARISONS {
                        return Err("A foreach if needs up to 256 count comparisons.".into());
                    }
                    for condition in comparisons {
                        read(&condition.source)?;
                    }
                    self.validate_steps(body, &mut bindings.clone(), validation, depth + 1)?;
                    continue;
                }
                Step::Accumulate { members } => {
                    verify_members(members)?;
                    validation.accumulated = true;
                    continue;
                }
            };
            if target == "_"
                || *target == self.item
                || *target == self.accumulator
                || self.inputs.contains_key(target)
                || !valid_name(target)
            {
                return Err(format!("Invalid loop-local target .{target}."));
            }
            validation.locals.insert(target);
            if validation.locals.len() > MAX_NAMES {
                return Err("A foreach body can use up to 64 loop-local names.".into());
            }
            bindings.insert(target);
        }
        Ok(())
    }

    pub(crate) fn run<'a, S: Source>(
        &self,
        stored: &HashMap<&str, Vec<Feature<'a, S>>>,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Vec<Feature<'a, S>>, String> {
        let mut work = Work { used: 0, check };
        let mut nodes = Vec::new();
        let mut by_feature: HashMap<FeatureId, usize> = HashMap::new();
        let mut source_sets = HashMap::new();
        let mut source_members = 0;
        for name in self.inputs() {
            if source_sets.contains_key(name) {
                continue;
            }
            let features = &stored[name];
            source_members += features.len();
            if source_members > MAX_INTERMEDIATE_FEATURES {
                return Err(MEMORY_LIMIT.into());
            }
            let mut members = Vec::with_capacity(features.len());
            for &feature in features {
                work.step()?;
                if feature.kind() != FeatureType::Node {
                    return Err(format!("Foreach input ${name} must contain only nodes."));
                }
                let id = feature.id();
                let index = if let Some(index) = by_feature.get(&id) {
                    *index
                } else {
                    let point = feature
                        .point()
                        .map_err(|error| format!("{id}: {error}"))?
                        .expect("node checked above");
                    let index = nodes.len();
                    nodes.push(Node {
                        feature,
                        position: NodePosition::new(point),
                    });
                    by_feature.insert(id, index);
                    index
                };
                members.push(index);
            }
            source_sets.insert(name, Nodes::new(members));
        }
        drop(by_feature);
        let mut run = Evaluation {
            nodes,
            inputs: self
                .inputs
                .iter()
                .map(|(alias, name)| (alias.as_str(), Rc::clone(&source_sets[name.as_str()])))
                .collect(),
            locals: HashMap::new(),
            local_members: 0,
            accumulator: Members::default(),
            work,
        };
        if let Some(initial) = self.initial.as_deref() {
            run.accumulator
                .extend(&source_sets[initial].members, &mut run.work)?;
        }
        for &item in &source_sets[self.source.as_str()].members {
            check()?;
            run.assign(&self.item, Nodes::new(vec![item]))?;
            run.steps(&self.body)?;
            run.locals.clear();
            run.local_members = 0;
        }
        check()?;
        Ok(run
            .accumulator
            .nodes
            .into_iter()
            .map(|i| run.nodes[i].feature)
            .collect())
    }
}

struct Node<'a, S> {
    feature: Feature<'a, S>,
    position: NodePosition,
}

type Tree = RTree<GeomWithData<[f64; 3], usize>>;

struct Nodes {
    members: Vec<usize>,
    tree: OnceCell<Tree>,
}

impl Nodes {
    fn new(members: Vec<usize>) -> Rc<Self> {
        Rc::new(Self {
            members,
            tree: OnceCell::new(),
        })
    }

    fn index<'a, S>(
        &'a self,
        nodes: &[Node<'_, S>],
        work: &mut Work<impl Fn() -> Result<(), String>>,
    ) -> Result<&'a Tree, String> {
        if self.tree.get().is_none() {
            let mut entries = Vec::with_capacity(self.members.len());
            for &i in &self.members {
                work.step()?;
                entries.push(GeomWithData::new(nodes[i].position.vector, i));
            }
            let tree = RTree::bulk_load(entries);
            (work.check)()?;
            let _ = self.tree.set(tree);
        }
        Ok(self.tree.get().expect("spatial index initialized"))
    }
}

#[derive(Default)]
struct Members {
    nodes: Vec<usize>,
    seen: HashSet<usize>,
}

impl Members {
    fn extend(
        &mut self,
        members: &[usize],
        work: &mut Work<impl Fn() -> Result<(), String>>,
    ) -> Result<(), String> {
        for &i in members {
            work.step()?;
            if self.seen.insert(i) {
                self.nodes.push(i);
            }
        }
        Ok(())
    }
}

struct Work<'a, C> {
    used: usize,
    check: &'a C,
}

impl<C: Fn() -> Result<(), String>> Work<'_, C> {
    fn step(&mut self) -> Result<(), String> {
        self.used += 1;
        if self.used > MAX_WORK {
            return Err(
                "Foreach exceeds the work budget. Narrow the input sets or reduce the radius."
                    .into(),
            );
        }
        if self.used.is_multiple_of(256) {
            (self.check)()?;
        }
        Ok(())
    }
}

struct Evaluation<'a, 'p, 'c, S, C> {
    nodes: Vec<Node<'a, S>>,
    inputs: HashMap<&'p str, Rc<Nodes>>,
    locals: HashMap<&'p str, Rc<Nodes>>,
    local_members: usize,
    accumulator: Members,
    work: Work<'c, C>,
}

impl<'p, S, C: Fn() -> Result<(), String>> Evaluation<'_, 'p, '_, S, C> {
    fn read(&self, name: &str) -> Rc<Nodes> {
        Rc::clone(
            self.locals
                .get(name)
                .or_else(|| self.inputs.get(name))
                .expect("validated foreach name"),
        )
    }

    fn assign(&mut self, name: &'p str, set: Rc<Nodes>) -> Result<(), String> {
        self.local_members += set.members.len();
        if let Some(previous) = self.locals.insert(name, set) {
            self.local_members -= previous.members.len();
        }
        if self.local_members > MAX_INTERMEDIATE_FEATURES {
            return Err(MEMORY_LIMIT.into());
        }
        Ok(())
    }

    fn steps(&mut self, steps: &'p [Step]) -> Result<(), String> {
        for step in steps {
            self.work.step()?;
            match step {
                Step::Nearby {
                    source,
                    reference,
                    meters,
                    target,
                } => {
                    let source = self.read(source);
                    let reference = self.read(reference);
                    let result = self.nearby(&source, &reference, *meters)?;
                    self.assign(target, Nodes::new(result))?;
                }
                Step::Copy { source, target } => self.assign(target, self.read(source))?,
                Step::Union { members, target } => {
                    let mut result = Members::default();
                    for name in members {
                        result.extend(&self.read(name).members, &mut self.work)?;
                    }
                    self.assign(target, Nodes::new(result.nodes))?;
                }
                Step::If { comparisons, body } => {
                    let mut matches = true;
                    for comparison in comparisons {
                        self.work.step()?;
                        if !comparison.matches(self.read(&comparison.source).members.len()) {
                            matches = false;
                            break;
                        }
                    }
                    if matches {
                        self.steps(body)?;
                    }
                }
                Step::Accumulate { members } => {
                    for name in members {
                        self.accumulator
                            .extend(&self.read(name).members, &mut self.work)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn nearby(
        &mut self,
        source: &Nodes,
        reference: &Nodes,
        meters: f64,
    ) -> Result<Vec<usize>, String> {
        if source.members.is_empty() || reference.members.is_empty() {
            return Ok(Vec::new());
        }
        let radius = NodeRadius::new(meters);
        let mut result = Vec::new();
        // Index the larger set. A persistent source keeps its tree across iterations.
        if source.members.len() < reference.members.len() {
            let index = reference.index(&self.nodes, &mut self.work)?;
            for &i in &source.members {
                self.work.step()?;
                let position = self.nodes[i].position;
                for entry in
                    index.locate_in_envelope_intersecting(&radius.envelope(position.vector))
                {
                    self.work.step()?;
                    if radius.includes(position, self.nodes[entry.data].position) {
                        result.push(i);
                        break;
                    }
                }
            }
        } else {
            let index = source.index(&self.nodes, &mut self.work)?;
            let mut seen = HashSet::new();
            for &i in &reference.members {
                self.work.step()?;
                let position = self.nodes[i].position;
                for entry in
                    index.locate_in_envelope_intersecting(&radius.envelope(position.vector))
                {
                    self.work.step()?;
                    let j = entry.data;
                    if !seen.contains(&j) && radius.includes(position, self.nodes[j].position) {
                        seen.insert(j);
                        result.push(j);
                    }
                }
                if result.len() == source.members.len() {
                    break;
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regol::{Bounds, Library, Query};
    use serde_json::json;
    use std::{cell::Cell, io};

    const STRESS: &[u8] = include_bytes!("../../tests/fixtures/stress.gol");

    struct Reads<'a>(&'a Cell<usize>);

    impl Source for Reads<'_> {
        fn len(&self) -> u64 {
            STRESS.len() as u64
        }

        fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
            self.0.set(self.0.get() + 1);
            STRESS.read_exact_at(offset, output)
        }
    }

    #[test]
    fn point_reads_are_linear_with_overlapping_imports_and_two_hop_neighborhoods() {
        let reads = Cell::new(0);
        let lib = Library::new(Reads(&reads)).unwrap();
        let query = Query::parse("n").unwrap();
        let prepared = query.prepare(&lib).unwrap();
        let features = prepared
            .in_bounds(Bounds::WORLD)
            .collect::<regol::Result<Vec<_>>>()
            .unwrap();
        assert!(features.len() > 500);
        let inputs = HashMap::from([("source", features.clone()), ("same", features.clone())]);
        let loop_: Loop = serde_json::from_value(json!({
            "source":"source", "item":"item", "inputs":{"all":"source","same":"same"},
            "accumulator":"accepted",
            "body":[
                {"type":"nearby", "source":"all", "reference":"item", "meters":0, "target":"one"},
                {"type":"nearby", "source":"same", "reference":"one", "meters":0, "target":"two"},
                {"type":"accumulate", "members":["two"]}
            ]
        }))
        .unwrap();
        loop_.validate().unwrap();
        reads.set(0);
        let result = loop_.run(&inputs, &|| Ok(())).unwrap();
        assert_eq!(result.len(), features.len());
        assert_eq!(reads.get(), features.len());
    }

    #[test]
    fn sparse_neighborhoods_reuse_the_index_and_visit_a_linear_number_of_candidates() {
        let lib = Library::new(STRESS).unwrap();
        let query = Query::parse("n").unwrap();
        let prepared = query.prepare(&lib).unwrap();
        let feature = prepared.in_bounds(Bounds::WORLD).next().unwrap().unwrap();
        let count = 8_192;
        let nodes = (0..count)
            .map(|i| Node {
                feature,
                position: NodePosition::new(regol::Point {
                    x: i * 100_000,
                    y: 0,
                }),
            })
            .collect();
        let mut eval = Evaluation {
            nodes,
            inputs: HashMap::new(),
            locals: HashMap::new(),
            local_members: 0,
            accumulator: Members::default(),
            work: Work {
                used: 0,
                check: &|| Ok(()),
            },
        };
        let source = Nodes::new((0..count as usize).collect());
        for i in 0..count as usize {
            let item = Nodes::new(vec![i]);
            assert_eq!(eval.nearby(&source, &item, 10.0).unwrap(), [i]);
        }
        assert_eq!(eval.work.used, 3 * count as usize);
        assert_eq!(source.tree.get().unwrap().size(), count as usize);
    }
}
