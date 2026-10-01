//! A text-preserving view of a JSON file, so rewriting Claude settings keeps
//! what the user wrote: key order, number text such as `1e3` or a 23-digit
//! integer, and string escapes. Edits are made on a `serde_json::Value`;
//! `reconcile` maps the edited value back onto the original document and
//! reuses the original text of every part that did not change.
//! serde_json's `preserve_order` and `arbitrary_precision` features are not
//! used: they are crate-wide, `arbitrary_precision` breaks numbers inside
//! untagged and flattened types (rmcp's JSON-RPC ids), and `preserve_order`
//! changes the bytes the Companion outbox hashes into its request ids.
//! Does NOT validate JSON; callers parse with serde_json first.

use serde_json::Value;

/// A JSON value with the original text of every scalar and key.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// A number, string, `true`, `false` or `null`, as written.
    Scalar(String),
    Array(Vec<Node>),
    /// Entries in file order: the key as written, its decoded text, the value.
    Object(Vec<(String, String, Node)>),
}

/// Parse text that serde_json has already accepted.
pub fn parse(text: &str) -> Result<Node, String> {
    let mut parser = Parser { text, at: 0 };
    let node = parser.value(0)?;
    parser.space();
    match parser.at == text.len() {
        true => Ok(node),
        false => Err("trailing text after the JSON value".into()),
    }
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
}

const MAX_DEPTH: usize = 256;

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn space(&mut self) {
        while self.peek().is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        self.space();
        match self.peek() == Some(byte) {
            true => {
                self.at += 1;
                Ok(())
            }
            false => Err(format!("expected '{}' at byte {}", byte as char, self.at)),
        }
    }

    fn value(&mut self, depth: usize) -> Result<Node, String> {
        if depth > MAX_DEPTH {
            return Err("JSON nests too deeply".into());
        }
        self.space();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Node::Scalar),
            Some(_) => Ok(Node::Scalar(self.bare())),
            None => Err("unexpected end of JSON".into()),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let start = self.at;
        self.at += 1;
        loop {
            match self.peek() {
                Some(b'\\') => self.at += 2,
                Some(b'"') => {
                    self.at += 1;
                    return self.text.get(start..self.at).map(str::to_string).ok_or("bad string".into());
                }
                Some(_) => self.at += 1,
                None => return Err("unterminated string".into()),
            }
        }
    }

    fn bare(&mut self) -> String {
        let start = self.at;
        while self.peek().is_some_and(|b| !matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
        self.text.get(start..self.at).unwrap_or_default().to_string()
    }

    fn array(&mut self, depth: usize) -> Result<Node, String> {
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Node::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Node::Array(items));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.at)),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Node, String> {
        self.at += 1;
        let mut entries = Vec::new();
        self.space();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Node::Object(entries));
        }
        loop {
            self.space();
            let raw = self.string()?;
            let key: String = serde_json::from_str(&raw).map_err(|_| "bad object key")?;
            self.expect(b':')?;
            entries.push((raw, key, self.value(depth + 1)?));
            self.space();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Node::Object(entries));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.at)),
            }
        }
    }
}

/// The node's value as serde_json reads it.
pub fn to_value(node: &Node) -> Value {
    match node {
        Node::Scalar(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
        Node::Array(items) => Value::Array(items.iter().map(to_value).collect()),
        Node::Object(entries) => {
            Value::Object(entries.iter().map(|(_, key, node)| (key.clone(), to_value(node))).collect())
        }
    }
}

/// A node for a value Wardwell wrote, in serde_json's text.
pub fn from_value(value: &Value) -> Node {
    match value {
        Value::Array(items) => Node::Array(items.iter().map(from_value).collect()),
        Value::Object(map) => Node::Object(
            map.iter()
                .map(|(key, value)| (serde_json::to_string(key).unwrap_or_default(), key.clone(), from_value(value)))
                .collect(),
        ),
        scalar => Node::Scalar(serde_json::to_string(scalar).unwrap_or_default()),
    }
}

/// `edited` laid over `original`: unchanged parts keep their original text
/// and order; kept keys keep their place; new keys follow; an array element
/// equal to an original element reuses it, and an edited object element is
/// reconciled with the original at its position.
pub fn reconcile(original: &Node, edited: &Value) -> Node {
    match (original, edited) {
        (Node::Object(entries), Value::Object(map)) => {
            let mut out = Vec::new();
            for (raw, key, node) in entries {
                if out.iter().any(|(_, seen, _): &(String, String, Node)| seen == key) {
                    continue;
                }
                if let Some(value) = map.get(key) {
                    out.push((raw.clone(), key.clone(), reconcile(node, value)));
                }
            }
            for (key, value) in map {
                if !entries.iter().any(|(_, seen, _)| seen == key) {
                    out.push((serde_json::to_string(key).unwrap_or_default(), key.clone(), from_value(value)));
                }
            }
            Node::Object(out)
        }
        (Node::Array(items), Value::Array(values)) => {
            let mut used = vec![false; items.len()];
            let mut out = Vec::new();
            for (index, value) in values.iter().enumerate() {
                let exact = items.iter().enumerate().position(|(j, item)| !used[j] && to_value(item) == *value);
                let same_place = (index < items.len() && !used[index] && matches!(items[index], Node::Object(_)) && value.is_object())
                    .then_some(index);
                match exact.or(same_place) {
                    Some(j) => {
                        used[j] = true;
                        out.push(reconcile(&items[j], value));
                    }
                    None => out.push(from_value(value)),
                }
            }
            Node::Array(out)
        }
        (Node::Scalar(_), value) if to_value(original) == *value => original.clone(),
        (_, value) => from_value(value),
    }
}

/// Two-space pretty text with a final newline, as serde_json writes it.
pub fn render(node: &Node) -> String {
    let mut out = String::new();
    write(node, 0, &mut out);
    out.push('\n');
    out
}

fn write(node: &Node, indent: usize, out: &mut String) {
    let pad = |n: usize| "  ".repeat(n);
    match node {
        Node::Scalar(raw) => out.push_str(raw),
        Node::Array(items) if items.is_empty() => out.push_str("[]"),
        Node::Object(entries) if entries.is_empty() => out.push_str("{}"),
        Node::Array(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                write(item, indent + 1, out);
                out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(indent));
            out.push(']');
        }
        Node::Object(entries) => {
            out.push_str("{\n");
            for (i, (raw, _, item)) in entries.iter().enumerate() {
                out.push_str(&pad(indent + 1));
                out.push_str(raw);
                out.push_str(": ");
                write(item, indent + 1, out);
                out.push_str(if i + 1 < entries.len() { ",\n" } else { "\n" });
            }
            out.push_str(&pad(indent));
            out.push('}');
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEXT: &str = "{\n  \"zeta\": 12345678901234567890123,\n  \"alpha\": 1e3,\n  \"esc\": \"a\\u0041\",\n  \"list\": [\n    {\n      \"b\": 1,\n      \"a\": 2\n    }\n  ]\n}\n";

    #[test]
    fn unchanged_text_round_trips_exactly() {
        let doc = parse(TEXT).unwrap();
        let value: Value = serde_json::from_str(TEXT).unwrap();
        assert_eq!(render(&reconcile(&doc, &value)), TEXT);
    }

    #[test]
    fn an_edit_keeps_everything_else_as_written() {
        let doc = parse(TEXT).unwrap();
        let mut value: Value = serde_json::from_str(TEXT).unwrap();
        value["list"][0]["c"] = json!(3);
        value["new"] = json!(true);
        let text = render(&reconcile(&doc, &value));
        assert!(text.starts_with("{\n  \"zeta\": 12345678901234567890123,\n  \"alpha\": 1e3,\n  \"esc\": \"a\\u0041\","), "{text}");
        assert!(text.contains("\"b\": 1,\n      \"a\": 2,\n      \"c\": 3"), "{text}");
        assert!(text.ends_with("  \"new\": true\n}\n"), "{text}");
    }

    #[test]
    fn removed_array_elements_leave_the_rest_in_place() {
        let text = "[\n  {\"k\": 1},\n  {\"k\": 2},\n  {\"k\": 3}\n]";
        let doc = parse(text).unwrap();
        let reconciled = reconcile(&doc, &json!([{"k": 1}, {"k": 3}]));
        assert_eq!(to_value(&reconciled), json!([{"k": 1}, {"k": 3}]));
    }

    #[test]
    fn parse_matches_serde_json() {
        let text = r#"{"a":[1,-2.5e-3,true,null,"x\"y",{}],"b":{"c":[]}}"#;
        assert_eq!(to_value(&parse(text).unwrap()), serde_json::from_str::<Value>(text).unwrap());
    }
}
