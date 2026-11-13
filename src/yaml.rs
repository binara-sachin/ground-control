//! A small YAML document tree that keeps line and column positions for every
//! node, built on `yaml-rust2`'s event parser. Validation errors point at the
//! exact place in `groundcontrol.yaml` (§5.1, §7.3).

use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};

/// 1-based line and column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl From<Marker> for Pos {
    fn from(m: Marker) -> Self {
        Pos { line: m.line(), col: m.col() + 1 }
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    /// A scalar; `quoted` is true when it was written in quotes or as a block.
    Scalar {
        text: String,
        quoted: bool,
    },
    Seq(Vec<Node>),
    /// Entries in source order. Duplicate keys are reported by the builder.
    Map(Vec<(Key, Node)>),
}

#[derive(Debug, Clone)]
pub struct Key {
    pub text: String,
    pub pos: Pos,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub value: Value,
    pub pos: Pos,
}

#[derive(Debug, Clone)]
pub struct ParseError {
    pub pos: Pos,
    pub message: String,
}

impl Node {
    pub fn as_str(&self) -> Option<&str> {
        match &self.value {
            Value::Scalar { text, .. } => Some(text),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self.value, Value::Null)
    }

    pub fn kind(&self) -> &'static str {
        match self.value {
            Value::Null => "null",
            Value::Scalar { .. } => "scalar",
            Value::Seq(_) => "list",
            Value::Map(_) => "mapping",
        }
    }
}

enum Frame {
    Seq(Pos, Vec<Node>),
    Map(Pos, Vec<(Key, Node)>, Option<Key>),
}

struct Builder {
    stack: Vec<Frame>,
    root: Option<Node>,
    errors: Vec<ParseError>,
    documents: usize,
}

impl Builder {
    fn push_node(&mut self, node: Node) {
        match self.stack.last_mut() {
            None => {
                if self.root.is_none() {
                    self.root = Some(node);
                }
            }
            Some(Frame::Seq(_, items)) => items.push(node),
            Some(Frame::Map(_, entries, pending)) => match pending.take() {
                None => {
                    let text = match &node.value {
                        Value::Scalar { text, .. } => text.clone(),
                        Value::Null => String::new(),
                        _ => {
                            self.errors.push(ParseError { pos: node.pos, message: "mapping keys must be plain strings".into() });
                            String::new()
                        }
                    };
                    if entries.iter().any(|(k, _)| k.text == text) {
                        self.errors.push(ParseError { pos: node.pos, message: format!("duplicate key `{text}`") });
                    }
                    *pending = Some(Key { text, pos: node.pos });
                }
                Some(key) => entries.push((key, node)),
            },
        }
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, ev: Event, mark: Marker) {
        let pos = Pos::from(mark);
        match ev {
            Event::DocumentStart => {
                self.documents += 1;
                if self.documents > 1 {
                    self.errors.push(ParseError { pos, message: "only one YAML document is allowed".into() });
                }
            }
            Event::Scalar(text, style, _, tag) => {
                let quoted = !matches!(style, TScalarStyle::Plain);
                let is_null =
                    !quoted && tag.is_none() && (text.is_empty() || text == "~" || text == "null" || text == "Null" || text == "NULL");
                let value = if is_null { Value::Null } else { Value::Scalar { text, quoted } };
                self.push_node(Node { value, pos });
            }
            Event::Alias(_) => {
                self.errors.push(ParseError { pos, message: "YAML aliases are not supported".into() });
                self.push_node(Node { value: Value::Null, pos });
            }
            Event::SequenceStart(..) => self.stack.push(Frame::Seq(pos, Vec::new())),
            Event::MappingStart(..) => self.stack.push(Frame::Map(pos, Vec::new(), None)),
            Event::SequenceEnd => {
                if let Some(Frame::Seq(p, items)) = self.stack.pop() {
                    self.push_node(Node { value: Value::Seq(items), pos: p });
                }
            }
            Event::MappingEnd => {
                if let Some(Frame::Map(p, entries, _)) = self.stack.pop() {
                    self.push_node(Node { value: Value::Map(entries), pos: p });
                }
            }
            _ => {}
        }
    }
}

/// Parses a single YAML document. An empty file is an empty mapping.
pub fn parse(src: &str) -> Result<Node, Vec<ParseError>> {
    let mut b = Builder { stack: Vec::new(), root: None, errors: Vec::new(), documents: 0 };
    let mut parser = Parser::new_from_str(src);
    if let Err(e) = parser.load(&mut b, true) {
        return Err(vec![ParseError { pos: Pos::from(*e.marker()), message: e.info().to_string() }]);
    }
    if !b.errors.is_empty() {
        return Err(b.errors);
    }
    Ok(b.root.unwrap_or(Node { value: Value::Map(Vec::new()), pos: Pos { line: 1, col: 1 } }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_one_based() {
        let n = parse("a:\n  b: hello\n").unwrap();
        let Value::Map(entries) = &n.value else { panic!() };
        let (k, v) = &entries[0];
        assert_eq!(k.pos, Pos { line: 1, col: 1 });
        let Value::Map(inner) = &v.value else { panic!() };
        assert_eq!(inner[0].1.pos, Pos { line: 2, col: 6 });
    }

    #[test]
    fn duplicate_keys_are_errors() {
        let err = parse("a: 1\na: 2\n").unwrap_err();
        assert!(err[0].message.contains("duplicate"));
        assert_eq!(err[0].pos.line, 2);
    }

    #[test]
    fn syntax_errors_have_positions() {
        let err = parse("a: [1, 2\nb: 3\n").unwrap_err();
        assert!(err[0].pos.line >= 1);
    }

    #[test]
    fn quoted_flags() {
        let n = parse("a: \"x\"\nb: y\nc:\n").unwrap();
        let Value::Map(e) = &n.value else { panic!() };
        assert!(matches!(e[0].1.value, Value::Scalar { quoted: true, .. }));
        assert!(matches!(e[1].1.value, Value::Scalar { quoted: false, .. }));
        assert!(e[2].1.is_null());
    }
}
