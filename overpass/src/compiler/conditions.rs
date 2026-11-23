use serde::Serialize;

use super::{
    Result,
    lex::{Tokens, is_decimal, is_quoted, is_word},
};

const MAX_DEPTH: usize = 32;
const MAX_NODES: usize = 128;

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(super) enum Condition<'text> {
    Tag {
        key: String,
    },
    Value {
        value: String,
    },
    Not {
        arg: Box<Self>,
    },
    Number {
        arg: Box<Self>,
    },
    IsNumber {
        arg: Box<Self>,
    },
    Binary {
        operator: &'text str,
        left: Box<Self>,
        right: Box<Self>,
    },
}

struct Expression<'text> {
    value: Condition<'text>,
    depth: usize,
}

fn precedence(operator: &str) -> Option<usize> {
    match operator {
        "||" => Some(1),
        "&&" => Some(2),
        "==" | "!=" => Some(3),
        "<" | "<=" | ">" | ">=" => Some(4),
        _ => None,
    }
}

pub(super) struct Conditions {
    remaining: usize,
}

impl Conditions {
    pub(super) fn new() -> Self {
        Self {
            remaining: MAX_NODES,
        }
    }

    pub(super) fn parse<'text>(&mut self, tokens: &mut Tokens<'text>) -> Result<Condition<'text>> {
        let tree = self.expression(tokens, 1, 0)?;
        if tokens.peek() != ")" {
            return Err(tokens.error("Expected ')' or a supported operator in (if:...)."));
        }
        Ok(tree.value)
    }

    fn node<'text>(
        &mut self,
        tokens: &Tokens<'text>,
        value: Condition<'text>,
        depth: usize,
    ) -> Result<Expression<'text>> {
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| tokens.error("Per-feature condition limit reached."))?;
        if depth > MAX_DEPTH {
            return Err(tokens.error("Per-feature condition nesting limit reached."));
        }
        Ok(Expression { value, depth })
    }

    fn term<'text>(
        &mut self,
        tokens: &mut Tokens<'text>,
        nesting: usize,
    ) -> Result<Expression<'text>> {
        if nesting == MAX_DEPTH {
            return Err(tokens.error("Per-feature condition nesting limit reached."));
        }
        let raw = tokens.peek();
        match raw {
            "!" => {
                tokens.expect("!")?;
                let arg = self.term(tokens, nesting + 1)?;
                self.node(
                    tokens,
                    Condition::Not {
                        arg: Box::new(arg.value),
                    },
                    arg.depth + 1,
                )
            }
            "(" => {
                tokens.expect("(")?;
                let inner = self.expression(tokens, 1, nesting + 1)?;
                tokens.expect(")")?;
                Ok(inner)
            }
            "t" => {
                tokens.expect("t")?;
                tokens.expect("[")?;
                if !is_quoted(tokens.peek()) && !is_word(tokens.peek()) {
                    return Err(tokens.error("Expected a tag key in t[...]."));
                }
                let key = tokens.literal(false)?;
                tokens.expect("]")?;
                self.node(tokens, Condition::Tag { key }, 1)
            }
            "number" | "is_number" => {
                tokens.expect(raw)?;
                tokens.expect("(")?;
                let inner = self.expression(tokens, 1, nesting + 1)?;
                tokens.expect(")")?;
                let arg = Box::new(inner.value);
                let value = if raw == "number" {
                    Condition::Number { arg }
                } else {
                    Condition::IsNumber { arg }
                };
                self.node(tokens, value, inner.depth + 1)
            }
            _ => {
                let value = if is_quoted(raw) {
                    tokens.literal(false)?
                } else if is_decimal(raw) {
                    tokens.expect(raw)?;
                    raw.to_owned()
                } else {
                    return Err(tokens.error(
                        "Expected a tag value, literal, number(...) or is_number(...) in (if:...).",
                    ));
                };
                self.node(tokens, Condition::Value { value }, 1)
            }
        }
    }

    fn expression<'text>(
        &mut self,
        tokens: &mut Tokens<'text>,
        minimum: usize,
        nesting: usize,
    ) -> Result<Expression<'text>> {
        let mut left = self.term(tokens, nesting)?;
        while let Some(priority) = precedence(tokens.peek()).filter(|&priority| priority >= minimum)
        {
            let operator = tokens.peek();
            tokens.expect(operator)?;
            let right = self.expression(tokens, priority + 1, nesting)?;
            left = self.node(
                tokens,
                Condition::Binary {
                    operator,
                    left: Box::new(left.value),
                    right: Box::new(right.value),
                },
                left.depth.max(right.depth) + 1,
            )?;
        }
        Ok(left)
    }
}
