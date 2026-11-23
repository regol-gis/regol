use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use super::{Binding, Result, lex::Tokens, unique};

const MAX_DEPTH: usize = 16;
const MAX_STATEMENTS: usize = 256;
const MAX_COMPARISONS: usize = 256;
const MAX_LOCALS: usize = 64;
const MAX_IMPORTS: usize = 256;
const MAX_UNION_MEMBERS: usize = 64;

#[derive(Serialize)]
pub(super) struct Loop<'text> {
    source: String,
    item: &'text str,
    inputs: BTreeMap<&'text str, String>,
    accumulator: &'text str,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial: Option<String>,
    body: Vec<Step<'text>>,
}

impl<'text> Loop<'text> {
    pub(super) fn accumulator(&self) -> &'text str {
        self.accumulator
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Step<'text> {
    Nearby {
        source: &'text str,
        reference: &'text str,
        meters: f64,
        target: &'text str,
    },
    Copy {
        source: &'text str,
        target: &'text str,
    },
    Union {
        members: Vec<&'text str>,
        target: &'text str,
    },
    If {
        comparisons: Vec<Comparison<'text>>,
        body: Vec<Step<'text>>,
    },
    Accumulate {
        members: Vec<&'text str>,
    },
}

#[derive(Serialize)]
struct Comparison<'text> {
    source: &'text str,
    op: &'text str,
    count: u32,
}

pub(super) fn parse<'text>(
    tokens: &mut Tokens<'text>,
    names: &HashMap<&'text str, Binding>,
    loop_locals: &mut HashSet<&'text str>,
) -> Result<Loop<'text>> {
    let input = named(tokens)?;
    let source = match names.get(input) {
        Some(binding) => binding.name.clone(),
        None if loop_locals.contains(input) => {
            return Err(tokens.error(format_args!(".{input} is local to its foreach block.")));
        }
        None => {
            return Err(tokens.error(format_args!("Define .{input} before using it in foreach.")));
        }
    };

    let item = target(tokens)?;
    if item == "_" || names.contains_key(item) {
        return Err(tokens.error("Choose a new named set for each foreach item."));
    }
    let mut parser = LoopParser {
        tokens,
        names,
        loop_locals,
        item,
        imports: BTreeMap::new(),
        locals: HashSet::from([item]),
        accumulator: None,
        statement_count: 0,
        comparison_count: 0,
    };
    let body = parser.body(HashSet::from([item]), 0)?;
    parser.tokens.take(";");
    let accumulator = parser.accumulator.ok_or_else(|| {
        parser
            .tokens
            .error("Accumulate a named set with a self-union inside foreach.")
    })?;
    let LoopParser {
        imports, locals, ..
    } = parser;
    loop_locals.extend(locals);

    Ok(Loop {
        source,
        item,
        inputs: imports,
        accumulator,
        initial: names.get(accumulator).map(|binding| binding.name.clone()),
        body,
    })
}

fn named<'text>(tokens: &mut Tokens<'text>) -> Result<&'text str> {
    tokens.expect(".")?;
    tokens.word()
}

fn target<'text>(tokens: &mut Tokens<'text>) -> Result<&'text str> {
    tokens.expect("->")?;
    named(tokens)
}

struct LoopParser<'text, 'parser> {
    tokens: &'parser mut Tokens<'text>,
    names: &'parser HashMap<&'text str, Binding>,
    loop_locals: &'parser HashSet<&'text str>,
    item: &'text str,
    imports: BTreeMap<&'text str, String>,
    locals: HashSet<&'text str>,
    accumulator: Option<&'text str>,
    statement_count: usize,
    comparison_count: usize,
}

impl<'text> LoopParser<'text, '_> {
    fn read(&mut self, alias: &'text str, bindings: &HashSet<&str>) -> Result<&'text str> {
        if bindings.contains(alias) {
            return Ok(alias);
        }
        match self.names.get(alias) {
            None if self.loop_locals.contains(alias) => Err(self
                .tokens
                .error(format_args!(".{alias} is local to its foreach block."))),
            Some(binding) if Some(alias) != self.accumulator => {
                if !self.imports.contains_key(alias) && self.imports.len() == MAX_IMPORTS {
                    return Err(self
                        .tokens
                        .error("A foreach body can use up to 256 input sets."));
                }
                self.imports.insert(alias, binding.name.clone());
                Ok(alias)
            }
            _ => Err(self.tokens.error(format_args!(
                "Define .{alias} before using it in this foreach block."
            ))),
        }
    }

    fn read_named(&mut self, bindings: &HashSet<&str>) -> Result<&'text str> {
        let alias = named(self.tokens)?;
        self.read(alias, bindings)
    }

    fn assign(&mut self, alias: &'text str, bindings: &mut HashSet<&'text str>) -> Result<()> {
        if alias == "_"
            || alias == self.item
            || Some(alias) == self.accumulator
            || self.names.contains_key(alias)
        {
            return Err(self.tokens.error(format_args!(
                "Choose a loop-local target other than .{alias}."
            )));
        }
        if !self.locals.contains(alias) && self.locals.len() == MAX_LOCALS {
            return Err(self
                .tokens
                .error("A foreach body can use up to 64 loop-local names."));
        }
        self.locals.insert(alias);
        bindings.insert(alias);
        Ok(())
    }

    fn condition(
        &mut self,
        bindings: &HashSet<&str>,
        depth: usize,
        comparisons: &mut Vec<Comparison<'text>>,
    ) -> Result<()> {
        if depth == MAX_DEPTH {
            return Err(self
                .tokens
                .error("Foreach condition nesting limit reached."));
        }
        loop {
            if self.tokens.take("(") {
                self.condition(bindings, depth + 1, comparisons)?;
                self.tokens.expect(")")?;
            } else {
                let alias = self.tokens.word()?;
                let source = self.read(alias, bindings)?;
                self.tokens.expect(".")?;
                self.tokens.expect("count")?;
                self.tokens.expect("(")?;
                self.tokens.expect("nodes")?;
                self.tokens.expect(")")?;
                let op = self.tokens.peek();
                if !matches!(op, "==" | "!=" | "<" | "<=" | ">" | ">=") {
                    return Err(self
                        .tokens
                        .error("Compare a named set's count(nodes) to a whole number."));
                }
                self.tokens.expect(op)?;
                let count = self.tokens.number()?;
                if count.fract() != 0.0 || !(0.0..=f64::from(u32::MAX)).contains(&count) {
                    return Err(self.tokens.error(
                        "Use a nonnegative whole number up to 4294967295 in a foreach comparison.",
                    ));
                }
                self.comparison_count += 1;
                if self.comparison_count > MAX_COMPARISONS {
                    return Err(self.tokens.error("Foreach comparison limit reached."));
                }
                comparisons.push(Comparison {
                    source,
                    op,
                    count: count as u32,
                });
            }
            if !self.tokens.take("&&") {
                return Ok(());
            }
        }
    }

    fn body(
        &mut self,
        mut bindings: HashSet<&'text str>,
        depth: usize,
    ) -> Result<Vec<Step<'text>>> {
        if depth == MAX_DEPTH {
            return Err(self.tokens.error("Foreach block nesting limit reached."));
        }
        self.tokens.expect("{")?;
        let mut steps = Vec::new();
        while !self.tokens.take("}") {
            self.statement_count += 1;
            if self.statement_count > MAX_STATEMENTS {
                return Err(self.tokens.error("Foreach statement limit reached."));
            }
            let step = match self.tokens.peek() {
                "if" => {
                    self.tokens.expect("if")?;
                    self.tokens.expect("(")?;
                    let mut comparisons = Vec::new();
                    self.condition(&bindings, 0, &mut comparisons)?;
                    self.tokens.expect(")")?;
                    let body = self.body(bindings.clone(), depth + 1)?;
                    self.tokens.take(";");
                    Step::If { comparisons, body }
                }
                "node" => {
                    self.tokens.expect("node")?;
                    let source = self.read_named(&bindings)?;
                    self.tokens.expect("(")?;
                    self.tokens.expect("around")?;
                    let reference = self.read_named(&bindings)?;
                    self.tokens.expect(":")?;
                    let meters = super::radius(self.tokens)?;
                    self.tokens.expect(")")?;
                    let target = target(self.tokens)?;
                    self.tokens.expect(";")?;
                    self.assign(target, &mut bindings)?;
                    Step::Nearby { source, reference, meters, target }
                }
                "(" => self.union(&mut bindings)?,
                "." => {
                    let source = self.read_named(&bindings)?;
                    let target = target(self.tokens)?;
                    self.tokens.expect(";")?;
                    self.assign(target, &mut bindings)?;
                    Step::Copy { source, target }
                }
                _ => return Err(self.tokens.error(
                    "Use named node proximity, named set assignments, set unions or if inside foreach."
                )),
            };
            steps.push(step);
        }
        Ok(steps)
    }

    fn union(&mut self, bindings: &mut HashSet<&'text str>) -> Result<Step<'text>> {
        self.tokens.expect("(")?;
        let mut members = Vec::new();
        loop {
            members.push(named(self.tokens)?);
            self.tokens.expect(";")?;
            if members.len() > MAX_UNION_MEMBERS {
                return Err(self
                    .tokens
                    .error("A foreach union can use up to 64 named sets."));
            }
            if self.tokens.take(")") {
                break;
            }
        }
        let target = target(self.tokens)?;
        self.tokens.expect(";")?;
        if members.contains(&target) {
            if self.accumulator.is_some_and(|existing| existing != target) {
                return Err(self
                    .tokens
                    .error("A foreach loop can expose one accumulating set."));
            }
            if target == "_" || self.locals.contains(target) || self.imports.contains_key(target) {
                return Err(self
                    .tokens
                    .error("Choose a separate named set for the foreach accumulator."));
            }
            self.accumulator = Some(target);
            members.retain(|&member| member != target);
            for &member in &members {
                self.read(member, bindings)?;
            }
            return Ok(Step::Accumulate {
                members: unique(members),
            });
        }
        for &member in &members {
            self.read(member, bindings)?;
        }
        self.assign(target, bindings)?;
        Ok(Step::Union {
            members: unique(members),
            target,
        })
    }
}
