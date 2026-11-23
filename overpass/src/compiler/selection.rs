use std::collections::HashSet;

use serde::Serialize;

use super::{
    AREA_SET_USAGE, Action, Binding, Parser, Result,
    conditions::{Condition, Conditions},
    lex::{Tokens, diagnostic, is_decimal, is_quoted, whitespace},
    radius,
    windows::{self, Window},
};

#[derive(Serialize)]
pub(super) struct Selection<'text> {
    kind: &'text str,
    tags: Vec<Tag>,
    #[serde(rename = "namedArea", skip_serializing_if = "Option::is_none")]
    named_area: Option<NamedArea>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conditions: Vec<Condition<'text>>,
    windows: Vec<Window>,
    bbox: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    polygon: Option<Vec<[f64; 2]>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    area: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    around: Option<AroundSelection>,
}

#[derive(Serialize)]
struct Tag {
    key: String,
    op: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    regex: Option<TagRegex>,
}

#[derive(Serialize)]
struct TagRegex {
    insensitive: bool,
    line: usize,
}

#[derive(Serialize)]
struct NamedArea {
    name: String,
}

#[derive(Serialize)]
pub(super) struct Membership<'text> {
    direction: &'text str,
    reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
}

#[derive(Serialize)]
struct AroundSelection {
    meters: f64,
    #[serde(flatten)]
    target: AroundTarget,
}

#[derive(Serialize)]
#[serde(untagged)]
enum AroundTarget {
    Reference { reference: String },
    Point { point: [f64; 2] },
    Polyline { polyline: Vec<[f64; 2]> },
}

struct Around {
    selection: AroundSelection,
    windows: Option<Vec<Window>>,
}

struct Polygon {
    points: Vec<[f64; 2]>,
    windows: Vec<Window>,
}

impl<'text> Parser<'text, '_> {
    pub(super) fn selection(&mut self, line: usize) -> Result<Binding> {
        if self.tokens.peek() == "<" {
            return Err(self
                .tokens
                .error("Upward recursion is unsupported in this mode."));
        }
        let kind = self.tokens.word()?;
        if !matches!(
            kind,
            "node" | "way" | "rel" | "relation" | "nw" | "nr" | "wr" | "nwr" | "area"
        ) {
            return Err(diagnostic(
                line,
                format_args!("Unsupported Overpass statement '{kind}'."),
            ));
        }
        let mut inputs = Vec::new();
        let mut input_names = HashSet::new();
        let mut tags = Vec::new();
        let mut boxes = None;
        let mut around = None;
        let mut membership = None;
        let mut area = None;
        let mut polygon = None;
        let mut conditions = Vec::new();
        let mut condition_parser = Conditions::new();
        loop {
            if self.tokens.take(".") {
                let name = self.tokens.word()?;
                let input = self.element_set(name, line)?;
                if input_names.insert(input.name.clone()) {
                    inputs.push(input.clone());
                }
            } else if self.tokens.take("[") {
                tags.push(tag(&mut self.tokens)?);
            } else if self.tokens.take("(") {
                match self.tokens.peek() {
                    "r" | "w" | "bn" | "bw" | "br" => {
                        let direction = self.tokens.word()?;
                        if membership.is_some() {
                            return Err(diagnostic(
                                line,
                                "Use one membership filter per selection.",
                            ));
                        }
                        membership = Some(self.membership(kind, direction, line)?);
                    }
                    "around" => {
                        self.tokens.expect("around")?;
                        if around.is_some() {
                            return Err(diagnostic(
                                line,
                                "Only one around filter per selection is supported.",
                            ));
                        }
                        around = Some(self.around(line)?);
                    }
                    "poly" => {
                        self.tokens.expect("poly")?;
                        if polygon.is_some() {
                            return Err(diagnostic(
                                line,
                                "Only one polygon filter per selection is supported.",
                            ));
                        }
                        self.tokens.expect(":")?;
                        polygon = Some(polygon_filter(&mut self.tokens)?);
                    }
                    "area" => {
                        self.tokens.expect("area")?;
                        if area.is_some() {
                            return Err(diagnostic(
                                line,
                                "Only one area filter per selection is supported.",
                            ));
                        }
                        let name = self.input_name()?;
                        area = Some(self.lookup(name, line)?.name.clone());
                    }
                    "if" => {
                        self.tokens.expect("if")?;
                        self.tokens.expect(":")?;
                        conditions.push(condition_parser.parse(&mut self.tokens)?);
                    }
                    _ => {
                        if boxes.is_some() {
                            return Err(self
                                .tokens
                                .error("Only one bounding box per selection is supported."));
                        }
                        boxes = Some(self.bounding_box()?);
                    }
                }
                self.tokens.expect(")")?;
            } else {
                break;
            }
        }
        let named_area = if kind == "area" {
            let name = tags
                .iter()
                .find(|tag| tag.key == "name" && tag.op == "eq")
                .and_then(|tag| tag.value.clone());
            if !inputs.is_empty()
                || membership.is_some()
                || boxes.is_some()
                || self.global_box.is_some()
                || around.is_some()
                || area.is_some()
                || polygon.is_some()
            {
                return Err(named_area_error(line));
            }
            Some(NamedArea {
                name: name.ok_or_else(|| named_area_error(line))?,
            })
        } else {
            None
        };
        if boxes.is_some() && self.global_box.is_some() {
            return Err(diagnostic(
                line,
                "Combining global and statement bounding boxes is unsupported.",
            ));
        }
        // Standalone membership statements are allowed to leave the global box.
        let standalone_membership = membership.is_some()
            && inputs.is_empty()
            && tags.is_empty()
            && conditions.is_empty()
            && around.is_none()
            && area.is_none()
            && polygon.is_none()
            && kind != "nwr";
        if boxes.is_none() && !standalone_membership {
            boxes.clone_from(&self.global_box);
        }
        if boxes.is_none()
            && inputs.is_empty()
            && membership.is_none()
            && around.is_none()
            && area.is_none()
            && polygon.is_none()
            && named_area.is_none()
        {
            return Err(diagnostic(
                line,
                "Bound this selection with a box, polygon, area, proximity filter or input set.",
            ));
        }
        let bbox = boxes.is_some();
        let (polygon, polygon_boxes) = match polygon {
            Some(polygon) => (Some(polygon.points), Some(polygon.windows)),
            None => (None, None),
        };
        let hints = boxes.or(polygon_boxes).or_else(|| {
            let around = around.as_ref()?;
            let windows = around.windows.as_ref()?;
            Some(windows::expanded(windows, around.selection.meters))
        });
        let binding_windows = hints
            .clone()
            .or_else(|| inputs.iter().find_map(|input| input.windows.clone()));
        let source = if membership.is_some() {
            String::new()
        } else if let Some(input) = inputs.first() {
            format!("${}", input.name)
        } else {
            "*".to_owned()
        };
        let intersect = inputs
            .iter()
            .skip(usize::from(membership.is_none()))
            .map(|input| input.name.clone())
            .collect();
        let only_nodes = kind == "node" || inputs.iter().any(|input| input.only_nodes);
        let selection = Selection {
            kind: match kind {
                "area" => "wr",
                "rel" => "relation",
                _ => kind,
            },
            named_area,
            tags,
            conditions,
            windows: hints.unwrap_or_default(),
            bbox,
            polygon,
            area,
            around: around.map(|around| around.selection),
        };
        let binding = self.save(
            source,
            Action::Selection {
                overpass: selection,
                membership,
                intersect,
            },
            binding_windows,
            only_nodes,
        );
        if kind == "area" {
            Ok(self.map_to_area(binding))
        } else {
            Ok(binding)
        }
    }

    fn element_set(&self, name: &str, line: usize) -> Result<&Binding> {
        let input = self.lookup(name, line)?;
        if input.mapped {
            return Err(diagnostic(line, AREA_SET_USAGE));
        }
        Ok(input)
    }

    fn membership(
        &mut self,
        kind: &str,
        direction: &'text str,
        line: usize,
    ) -> Result<Membership<'text>> {
        let relation = matches!(kind, "rel" | "relation");
        if direction == "w" && kind != "node" {
            return Err(diagnostic(
                line,
                "Use node(w) to select member nodes of ways.",
            ));
        }
        if direction == "bn" && !relation && !matches!(kind, "way" | "wr") {
            return Err(diagnostic(
                line,
                "Use way(bn), rel(bn) or wr(bn) to select parents of nodes.",
            ));
        }
        if matches!(direction, "bw" | "br") && !relation {
            return Err(diagnostic(
                line,
                format_args!("Use rel({direction}) to select parent relations."),
            ));
        }
        let input = self.input_name()?;
        let reference = self.element_set(input, line)?.name.clone();
        let role = if self.tokens.take(":") {
            if direction == "w" || (direction == "bn" && !relation) {
                return Err(diagnostic(
                    line,
                    "Position and role restrictions on direct way and node membership are unsupported.",
                ));
            }
            Some(self.tokens.literal(false)?)
        } else {
            None
        };
        Ok(Membership {
            direction,
            reference,
            role,
        })
    }

    fn around(&mut self, line: usize) -> Result<Around> {
        let reference = if self.tokens.take(".") {
            Some(self.tokens.word()?)
        } else {
            None
        };
        // Bare `around:` uses the default set and can take coordinates, while
        // an explicitly named set (including `._`) cannot be combined with them.
        self.tokens.expect(":")?;
        let meters = radius(&mut self.tokens)?;
        let (target, windows) = if self.tokens.take(",") {
            if reference.is_some() {
                return Err(self.tokens.error("Choose a reference set or coordinates."));
            }
            self.around_positions()?
        } else {
            let input = self.element_set(reference.unwrap_or("_"), line)?;
            let windows = if input.only_nodes {
                input.windows.clone()
            } else {
                None
            };
            (
                AroundTarget::Reference {
                    reference: input.name.clone(),
                },
                windows,
            )
        };
        Ok(Around {
            selection: AroundSelection { meters, target },
            windows,
        })
    }

    fn around_positions(&mut self) -> Result<(AroundTarget, Option<Vec<Window>>)> {
        let mut positions = Vec::new();
        loop {
            if positions.len() == 10_000 {
                return Err(self
                    .tokens
                    .error("Around accepts at most 10,000 latitude and longitude pairs."));
            }
            let lat = self.tokens.number()?;
            self.tokens.expect(",")?;
            let lon = self.tokens.number()?;
            if lat.abs() > 90.0 || lon.abs() > 180.0 {
                return Err(self.tokens.error("Invalid proximity coordinates."));
            }
            positions.push([lon, lat]);
            if !self.tokens.take(",") {
                break;
            }
        }
        if let [[lon, lat]] = positions.as_slice() {
            let point = [*lon, *lat];
            Ok((
                AroundTarget::Point { point },
                Some(vec![[*lon, *lat, *lon, *lat]]),
            ))
        } else {
            // The executor follows the full great-circle arcs.
            Ok((
                AroundTarget::Polyline {
                    polyline: positions,
                },
                None,
            ))
        }
    }
}

fn named_area_error(line: usize) -> String {
    diagnostic(
        line,
        "Named areas need an exact [name=\"…\"] filter. Apply any bounding box to the following element selection.",
    )
}

fn tag(tokens: &mut Tokens<'_>) -> Result<Tag> {
    let absent = tokens.take("!");
    let key = tokens.literal(false)?;
    let op = match tokens.peek() {
        "=" => Some("eq"),
        "!=" => Some("ne"),
        "~" => Some("re"),
        "!~" => Some("nre"),
        _ => None,
    };
    if op.is_some() {
        tokens.expect(tokens.peek())?;
    }
    if absent && op.is_some() {
        return Err(tokens.error("Invalid negated tag filter."));
    }
    let is_regex = matches!(op, Some("re" | "nre"));
    let line = tokens.line();
    let value = op.map(|_| tokens.literal(is_regex)).transpose()?;
    let insensitive = is_regex && tokens.take(",");
    if insensitive {
        tokens.expect("i")?;
    }
    tokens.expect("]")?;
    Ok(Tag {
        key,
        op: op.unwrap_or(if absent { "missing" } else { "exists" }),
        value,
        regex: is_regex.then_some(TagRegex { insensitive, line }),
    })
}

fn polygon_filter(tokens: &mut Tokens<'_>) -> Result<Polygon> {
    if !is_quoted(tokens.peek()) {
        return Err(tokens.error("Quote the polygon's latitude and longitude pairs."));
    }
    let coordinates = tokens.literal(false)?;
    let fields: Vec<_> = coordinates
        .split(whitespace)
        .filter(|field| !field.is_empty())
        .collect();
    if fields.len() < 6 || fields.len() % 2 != 0 || fields.len() > 20_000 {
        return Err(tokens.error("A polygon needs 3 to 10,000 latitude and longitude pairs."));
    }
    let mut points = Vec::with_capacity(fields.len() / 2);
    let mut distinct = HashSet::new();
    let decimal = |field: &str| {
        is_decimal(field)
            .then(|| field.parse::<f64>().ok())
            .flatten()
    };
    for fields in fields.chunks_exact(2) {
        let point = decimal(fields[0]).zip(decimal(fields[1]));
        let Some((lat, lon)) = point.filter(|&(lat, lon)| lat.abs() <= 90.0 && lon.abs() <= 180.0)
        else {
            return Err(tokens.error("Invalid polygon coordinates."));
        };
        let key = |value: f64| if value == 0.0 { 0 } else { value.to_bits() };
        distinct.insert([key(lon), key(lat)]);
        points.push([lon, lat]);
    }
    if distinct.len() < 3 {
        return Err(tokens.error("A polygon needs at least three distinct points."));
    }
    let windows = windows::polygon(&points)
        .ok_or_else(|| tokens.error("A polygon ring may wrap around the world at most once."))?;
    Ok(Polygon { points, windows })
}
