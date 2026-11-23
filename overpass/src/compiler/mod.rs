//! Compile the supported Overpass QL subset to Live's ordered set plan.

mod conditions;
mod control;
mod lex;
mod selection;
mod windows;

use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use serde::Serialize;
use serde_json::Value;

use self::{
    lex::{Tokens, diagnostic},
    selection::{Membership, Selection},
    windows::Window,
};

type Result<T> = std::result::Result<T, String>;

const AREA_SET_USAGE: &str =
    "Use area sets in an (area) filter, area union or difference, or with out count.";
const MAX_SET_DEPTH: usize = 32;

/// Compile QL to the serialized plan shared with Live. Viewport windows are
/// consulted only for `{{bbox}}` and use west, south, east, north order.
pub fn compile(text: &str, viewport: Option<&[[f64; 4]]>) -> Result<Value> {
    let mut parser = Parser {
        tokens: Tokens::new(text)?,
        viewport,
        timeout: None,
        global_box: None,
        sets: Vec::new(),
        outputs: Vec::new(),
        names: HashMap::new(),
        loop_locals: HashSet::new(),
    };
    parser.settings()?;
    while !parser.tokens.peek().is_empty() {
        if parser.tokens.peek() == "out"
            || (parser.tokens.peek() == "." && parser.tokens.ahead(2) == Some("out"))
        {
            parser.output()?;
        } else if parser.tokens.take("foreach") {
            parser.foreach()?;
        } else {
            parser.statement(0)?;
        }
    }
    let Some(first) = parser.outputs.first() else {
        return Err(parser
            .tokens
            .error("Finish with out geom; to display a set."));
    };
    // Preserve Live's streaming single-output request shape.
    let display = if parser.outputs.len() == 1 && first.detail.is_none() && first.order.is_none() {
        Display::Single {
            show: [first.source.clone()],
            output: first.mode,
            output_name: first.name,
        }
    } else {
        Display::Ordered {
            show: [],
            outputs: parser.outputs,
        }
    };
    serde_json::to_value(Plan {
        sets: parser.sets,
        display,
        timeout: parser.timeout,
    })
    .map_err(|error| error.to_string())
}

#[derive(Serialize)]
struct Plan<'text> {
    sets: Vec<Set<'text>>,
    #[serde(flatten)]
    display: Display<'text>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout: Option<u32>,
}

#[derive(Serialize)]
#[serde(untagged, rename_all_fields = "camelCase")]
enum Display<'text> {
    Single {
        show: [String; 1],
        output: &'text str,
        output_name: &'text str,
    },
    Ordered {
        show: [String; 0],
        outputs: Vec<Output<'text>>,
    },
}

#[derive(Serialize)]
struct Output<'text> {
    after: usize,
    source: String,
    name: &'text str,
    mode: &'text str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'text str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    order: Option<&'text str>,
}

#[derive(Serialize)]
struct Set<'text> {
    name: String,
    source: String,
    #[serde(flatten)]
    action: Action<'text>,
    filters: [(); 0],
}

#[derive(Serialize)]
#[serde(untagged)]
enum Action<'text> {
    Selection {
        overpass: Selection<'text>,
        #[serde(skip_serializing_if = "Option::is_none")]
        membership: Option<Membership<'text>>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        intersect: Vec<String>,
    },
    MapToArea {
        map_to_area: bool,
    },
    Recurse {
        recurse: String,
    },
    Difference {
        difference: Vec<String>,
    },
    Union {
        union: Vec<String>,
    },
    Control {
        control: control::Loop<'text>,
    },
}

#[derive(Clone)]
struct Binding {
    name: String,
    windows: Option<Vec<Window>>,
    only_nodes: bool,
    mapped: bool,
}

struct Parser<'text, 'viewport> {
    tokens: Tokens<'text>,
    viewport: Option<&'viewport [Window]>,
    global_box: Option<Vec<Window>>,
    timeout: Option<u32>,
    sets: Vec<Set<'text>>,
    outputs: Vec<Output<'text>>,
    names: HashMap<&'text str, Binding>,
    loop_locals: HashSet<&'text str>,
}

impl<'text> Parser<'text, '_> {
    fn settings(&mut self) -> Result<()> {
        while self.tokens.take("[") {
            let setting = self.tokens.word()?;
            self.tokens.expect(":")?;
            match setting {
                "out" => self.tokens.expect("json")?,
                "bbox" => self.global_box = Some(self.bounding_box()?),
                "timeout" => {
                    let timeout = self.tokens.number()?;
                    if timeout.fract() != 0.0 || !(1.0..=2_147_483.0).contains(&timeout) {
                        return Err(self
                            .tokens
                            .error("Timeout must be a positive whole number of seconds."));
                    }
                    self.timeout = Some(timeout as u32);
                }
                _ => {
                    return Err(self
                        .tokens
                        .error(format_args!("Unsupported setting '{setting}'.")));
                }
            }
            self.tokens.expect("]")?;
            if self.tokens.peek() != "[" {
                self.tokens.expect(";")?;
                break;
            }
        }
        Ok(())
    }

    fn bounding_box(&mut self) -> Result<Vec<Window>> {
        let line = self.tokens.line();
        if self.tokens.take("{{bbox}}") {
            let windows = self
                .viewport
                .filter(|windows| !windows.is_empty())
                .ok_or_else(|| diagnostic(line, "Supply --bbox for the {{bbox}} placeholder."))?;
            if windows.len() > 2
                || windows
                    .iter()
                    .any(|&window| crate::geometry::project_box(window).is_err())
            {
                return Err(diagnostic(
                    line,
                    "Invalid viewport for the {{bbox}} placeholder.",
                ));
            }
            return Ok(windows.to_vec());
        }
        let south = self.tokens.number()?;
        self.tokens.expect(",")?;
        let west = self.tokens.number()?;
        self.tokens.expect(",")?;
        let north = self.tokens.number()?;
        self.tokens.expect(",")?;
        let east = self.tokens.number()?;
        if south < -90.0
            || north > 90.0
            || south > north
            || west.abs() > 180.0
            || east.abs() > 180.0
        {
            return Err(self
                .tokens
                .error("Invalid bounding box. Use south, west, north, east."));
        }
        Ok(if west > east {
            vec![[west, south, 180.0, north], [-180.0, south, east, north]]
        } else {
            vec![[west, south, east, north]]
        })
    }

    fn lookup(&self, name: &str, line: usize) -> Result<&Binding> {
        self.names.get(name).ok_or_else(|| {
            if self.loop_locals.contains(name) {
                diagnostic(line, format_args!(".{name} is local to foreach."))
            } else {
                diagnostic(line, format_args!("Define .{name} before using it."))
            }
        })
    }

    fn input_name(&mut self) -> Result<&'text str> {
        if self.tokens.take(".") {
            self.tokens.word()
        } else {
            Ok("_")
        }
    }

    fn save(
        &mut self,
        source: String,
        action: Action<'text>,
        windows: Option<Vec<Window>>,
        only_nodes: bool,
    ) -> Binding {
        let name = format!("set{}", self.sets.len() + 1);
        self.sets.push(Set {
            name: name.clone(),
            source,
            action,
            filters: [],
        });
        Binding {
            name,
            windows,
            only_nodes,
            mapped: false,
        }
    }

    fn map_to_area(&mut self, input: Binding) -> Binding {
        let mut result = self.save(
            format!("${}", input.name),
            Action::MapToArea { map_to_area: true },
            None,
            false,
        );
        result.mapped = true;
        result
    }

    fn recurse_down(&mut self, input: Binding, line: usize) -> Result<Binding> {
        if self.tokens.peek() == ">" {
            return Err(diagnostic(
                line,
                "Transitive recursion is unsupported in this mode.",
            ));
        }
        if input.mapped {
            return Err(diagnostic(line, AREA_SET_USAGE));
        }
        Ok(self.save(
            String::new(),
            Action::Recurse {
                recurse: input.name,
            },
            None,
            false,
        ))
    }

    fn statement(&mut self, depth: usize) -> Result<Binding> {
        let line = self.tokens.line();
        let result = if self.tokens.take("(") {
            self.block(depth, line)?
        } else if self.tokens.take(".") {
            let alias = self.tokens.word()?;
            let input = self.lookup(alias, line)?.clone();
            if self.tokens.take("map_to_area") {
                self.map_to_area(input)
            } else if self.tokens.take(">") {
                self.recurse_down(input, line)?
            } else {
                input
            }
        } else if self.tokens.take("map_to_area") {
            let input = self.lookup("_", line)?.clone();
            self.map_to_area(input)
        } else if self.tokens.take(">") {
            let input = self.lookup("_", line)?.clone();
            self.recurse_down(input, line)?
        } else {
            self.selection(line)?
        };
        let target = if self.tokens.take("->") {
            self.tokens.expect(".")?;
            self.tokens.word()?
        } else {
            "_"
        };
        self.tokens.expect(";")?;
        self.names.insert(target, result.clone());
        Ok(result)
    }

    fn block(&mut self, depth: usize, line: usize) -> Result<Binding> {
        if depth >= MAX_SET_DEPTH {
            return Err(diagnostic(line, "Set block nesting limit reached."));
        }
        let mut members = Vec::new();
        let mut difference = false;
        while !self.tokens.take(")") {
            if self.tokens.peek().is_empty() {
                return Err(diagnostic(line, "Unclosed set block."));
            }
            if difference || (self.tokens.peek() == "-" && members.len() != 1) {
                return Err(self
                    .tokens
                    .error("A difference block needs exactly two statements."));
            }
            if self.tokens.take("-") {
                difference = true;
                if self.tokens.peek() == ")" {
                    return Err(self.tokens.error("Expected a statement after '-'."));
                }
            }
            members.push(self.statement(depth + 1)?);
        }
        let Some(first) = members.first() else {
            return Err(diagnostic(line, "A union needs at least one selection."));
        };
        let mapped = first.mapped;
        if members.iter().any(|member| member.mapped != mapped) {
            return Err(diagnostic(
                line,
                "A set block must contain either all area sets or all ordinary element sets.",
            ));
        }
        let references = members.iter().map(|member| member.name.clone());
        let (action, windows, only_nodes) = if difference {
            (
                Action::Difference {
                    difference: references.collect(),
                },
                first.windows.clone(),
                first.only_nodes,
            )
        } else {
            let windows = members
                .iter()
                .all(|member| member.windows.is_some())
                .then(|| {
                    windows::compact(
                        members
                            .iter()
                            .flat_map(|member| member.windows.iter().flatten().copied()),
                    )
                });
            (
                Action::Union {
                    union: unique(references),
                },
                windows,
                members.iter().all(|member| member.only_nodes),
            )
        };
        let mut binding = self.save(String::new(), action, windows, only_nodes);
        binding.mapped = mapped;
        Ok(binding)
    }

    fn foreach(&mut self) -> Result<()> {
        let control = control::parse(&mut self.tokens, &self.names, &mut self.loop_locals)?;
        let accumulator = control.accumulator();
        let result = self.save(String::new(), Action::Control { control }, None, true);
        self.names.insert(accumulator, result);
        Ok(())
    }

    fn output(&mut self) -> Result<()> {
        let line = self.tokens.line();
        let name = self.input_name()?;
        self.tokens.expect("out")?;
        let mut mode = None;
        let mut detail = None;
        let mut order = None;
        let count_error = || diagnostic(line, "Use out count without other output modifiers.");
        if self.tokens.take("count") {
            if !self.tokens.take(";") {
                return Err(count_error());
            }
            mode = Some("count");
        } else {
            while !self.tokens.take(";") {
                let modifier = self.tokens.peek();
                let (slot, duplicate) = match modifier {
                    "geom" | "center" => (&mut mode, "Choose one output geometry mode."),
                    "body" | "skel" => (&mut detail, "Choose one output detail level."),
                    "asc" | "qt" => (&mut order, "Choose one output order."),
                    "count" => return Err(count_error()),
                    "" => {
                        return Err(diagnostic(
                            line,
                            "Finish the out statement with a semicolon.",
                        ));
                    }
                    _ => {
                        return Err(self
                            .tokens
                            .error(format_args!("Unsupported out modifier '{modifier}'.")));
                    }
                };
                if slot.is_some() {
                    return Err(diagnostic(line, duplicate));
                }
                *slot = Some(self.tokens.word()?);
            }
        }
        let source = self.lookup(name, line)?;
        if source.mapped && mode != Some("count") {
            return Err(diagnostic(line, AREA_SET_USAGE));
        }
        self.outputs.push(Output {
            after: self.sets.len(),
            source: source.name.clone(),
            name,
            mode: mode.unwrap_or("geom"),
            detail,
            order,
        });
        Ok(())
    }
}

fn radius(tokens: &mut Tokens<'_>) -> Result<f64> {
    let meters = tokens.number()?;
    if meters < 0.0 {
        return Err(tokens.error("Radius must be nonnegative."));
    }
    Ok(meters)
}

fn unique<T: Clone + Eq + Hash>(values: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}
