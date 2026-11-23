//! Locate invalid draft values and place diagnostics in the source JSON.
use super::*;

/// One step of a JSON path: an object key or an array index.
#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Key(String),
    Index(usize),
}
/// `steps.read.op`, `triggers[0].event`.
fn render(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}
enum Frame {
    Object(Option<String>),
    Array(usize),
}
fn segments(stack: &[Frame]) -> Vec<Seg> {
    stack
        .iter()
        .filter_map(|frame| match frame {
            Frame::Object(Some(key)) => Some(Seg::Key(key.clone())),
            Frame::Array(i) => Some(Seg::Index(*i)),
            Frame::Object(None) => None,
        })
        .collect()
}
/// Walks `text` up to byte `stop`, or until the value at `target` starts.
/// Returns the path of the value being read at the stop, and the offset of
/// the first byte of the value at `target`. An error serde reports once a
/// whole object was read (it reads a tagged operation or expression whole
/// before checking it) names the object just closed.
fn scan(text: &str, stop: usize, target: Option<&[Seg]>) -> (Vec<Seg>, Option<usize>) {
    let mut cursor = JsonCursor::default();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < stop {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let key = b == b'"' && matches!(cursor.stack.last(), Some(Frame::Object(None)));
        if cursor.at_target(b, key, target) {
            return (segments(&cursor.stack), Some(i));
        }
        i = cursor.advance(text, i, key);
    }
    (
        cursor.closed.unwrap_or_else(|| segments(&cursor.stack)),
        None,
    )
}
#[derive(Default)]
struct JsonCursor {
    stack: Vec<Frame>,
    closed: Option<Vec<Seg>>,
    previous: Option<u8>,
}
impl JsonCursor {
    fn at_target(&self, b: u8, key: bool, target: Option<&[Seg]>) -> bool {
        let value = !key
            && !matches!(b, b'}' | b']' | b',' | b':')
            && matches!(self.previous, None | Some(b':' | b'[' | b','));
        value && target.is_some_and(|t| segments(&self.stack) == t)
    }
    fn advance(&mut self, text: &str, mut i: usize, key: bool) -> usize {
        let bytes = text.as_bytes();
        let b = bytes[i];
        match b {
            b'{' => self.stack.push(Frame::Object(None)),
            b'[' => self.stack.push(Frame::Array(0)),
            b'}' | b']' => {
                self.closed = Some(segments(&self.stack[..self.stack.len().saturating_sub(1)]));
                self.stack.pop();
            }
            b',' => match self.stack.last_mut() {
                Some(Frame::Object(key)) => *key = None,
                Some(Frame::Array(index)) => *index += 1,
                None => {}
            },
            b'"' => {
                let start = i + 1;
                i = quote_end(bytes, i);
                if key && let Some(Frame::Object(slot)) = self.stack.last_mut() {
                    let raw = &text[start..i.min(bytes.len())];
                    *slot = Some(serde_json::from_str(&format!("\"{raw}\"")).unwrap_or(raw.into()));
                }
            }
            _ => {}
        }
        if !matches!(b, b'}' | b']' | b',') {
            self.closed = None;
        }
        self.previous = Some(b);
        i + 1
    }
}
fn quote_end(bytes: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < bytes.len() && bytes[i] != b'"' {
        i += if bytes[i] == b'\\' { 2 } else { 1 };
    }
    i
}
#[cfg(test)]
fn json_path_at(text: &str, offset: usize) -> String {
    render(&scan(text, offset, None).0)
}
/// The value at `path` inside `value`.
fn at_path<'a>(value: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, seg| match seg {
        Seg::Key(key) => v.get(key),
        Seg::Index(i) => v.get(i),
    })
}
/// For a tagged object (an operation, expression or schema): `Some(None)`
/// when it parses as one, `Some(Some(error))` when it does not. `None` when
/// `value` is not tagged.
fn tagged_error(value: &Value) -> Option<Option<String>> {
    let tag = |key: &str| value.get(key).is_some_and(Value::is_string);
    let error = if tag("op") {
        serde_json::from_value::<Expr>(value.clone()).err()
    } else if tag("kind") {
        DraftOperation::deserialize(value.clone()).err()
    } else if tag("type") {
        serde_json::from_value::<Schema>(value.clone()).err()
    } else {
        return None;
    };
    Some(error.map(|e| e.to_string()))
}
/// Extends `path` to the deepest tagged object under `value` whose own parse
/// error is `reason`, serde's message, and returns whether it found one.
/// Data is never entered: a literal's `value` and a lookup case's `when`
/// hold the owner's values, which may look like anything.
fn deepest_fault(value: &Value, reason: &str, path: &mut Vec<Seg>) -> bool {
    let children = children(value);
    for (seg, child) in children {
        path.push(seg);
        let own = tagged_error(child);
        // A tagged object that parses holds no fault.
        if own != Some(None) && deepest_fault(child, reason, path) {
            return true;
        }
        if matches!(&own, Some(Some(error)) if error == reason) {
            return true;
        }
        path.pop();
    }
    false
}
fn children(value: &Value) -> Vec<(Seg, &Value)> {
    let literal = value.get("op").and_then(Value::as_str) == Some("literal");
    let case = value.get("when").is_some() && value.get("then").is_some();
    match value {
        Value::Object(map) => map
            .iter()
            .filter(|(k, _)| {
                let data = (literal && *k == "value") || (case && *k == "when");
                !data
            })
            .map(|(k, v)| (Seg::Key(k.clone()), v))
            .collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (Seg::Index(i), v))
            .collect(),
        _ => Vec::new(),
    }
}
/// 1-based line and column of byte `offset`.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text.as_bytes()[..offset.min(text.len())];
    let line = before.iter().filter(|b| **b == b'\n').count() + 1;
    let column = before.len()
        - before
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |p| p + 1)
        + 1;
    (line, column)
}

/// A contract position holding anything but a name: refused where it is
/// written, before serde, which reads an operation whole and so cannot say
/// which of its fields is wrong.
pub(super) fn contract_positions(draft: &Value) -> Vec<Diagnostic> {
    let steps = draft
        .get("steps")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, step)| {
            invalid_reference(step).map(|field| format!("steps.{name}.op.{field}"))
        });
    let triggers = draft
        .get("triggers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, trigger)| trigger.get("event").is_some_and(|v| !v.is_string()))
        .map(|(i, _)| format!("triggers[{i}].event"));
    steps.chain(triggers)

        .map(|path| {
            Diagnostic::error(
                "draft_format",
                &at(DRAFT_FILE, &path),
                format!(
                    "{path}: expected a contract name. Write the contract name as a string, such as \"work.read\"; the tool fills in its identity."
                ),
            )
        })
        .collect()
}

fn invalid_reference(step: &Value) -> Option<&'static str> {
    let op = step.get("op")?;
    match op.get("kind")?.as_str()? {
        "invoke" if !op.get("command")?.is_string() => Some("command"),
        "wait" if !matches!(op.get("event")?, Value::String(_) | Value::Null) => Some("event"),
        _ => None,
    }
}
/// Strict JSON with the place of the first fault. With `positions`, line and
/// column are those of `text`, the file the agent wrote. With `parsed` (the
/// same text as a value), a fault serde reports at the end of a step is moved
/// to the operation, expression or schema inside it that is wrong.
pub(super) fn parse<T: DeserializeOwned>(
    file: &str,
    text: &str,
    positions: bool,
    parsed: Option<&Value>,
) -> Result<T, Diagnostic> {
    serde_json::from_str(text).map_err(|e| {
        let (mut line, mut column) = (e.line(), e.column());
        let offset: usize = text
            .split_inclusive('\n')
            .take(line.saturating_sub(1))
            .map(str::len)
            .sum::<usize>()
            + column;
        let full = e.to_string();
        let reason = full
            .rsplit_once(" at line ")
            .map_or(full.as_str(), |(reason, _)| reason);
        let mut path = scan(text, offset, None).0;
        if let Some(start) = fault_offset(parsed, text, reason, &mut path) {
            (line, column) = line_column(text, start);
        }
        let path = render(&path);
        let place = match (positions, path.is_empty()) {
            (true, true) => format!("{file} line {line} column {column}"),
            (true, false) => format!("{file} line {line} column {column} ({path})"),
            (false, true) => file.to_string(),
            (false, false) => format!("{file} {path}"),
        };
        let code = format!("{}_format", file.trim_end_matches(".json"));
        let mut d = Diagnostic::error(&code, &at(file, &path), format!("{place}: {reason}."));
        if positions {
            d.line = u32::try_from(line).ok();
            d.column = u32::try_from(column).ok();
        }
        d
    })
}

fn fault_offset(
    parsed: Option<&Value>,
    text: &str,
    reason: &str,
    path: &mut Vec<Seg>,
) -> Option<usize> {
    let inner = at_path(parsed?, path)?;
    if !deepest_fault(inner, reason, path) {
        return None;
    }
    scan(text, text.len(), Some(path)).1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_paths_name_object_keys_and_array_items() {
        let text = "{\"steps\": {\"a\": {\"op\": 1}}, \"triggers\": [{\"id\": 1}, {\"event\": 2}]}";
        let offset = |needle: &str| text.find(needle).unwrap() + needle.len();
        assert_eq!(json_path_at(text, offset("\"op\"")), "steps.a.op");
        assert_eq!(json_path_at(text, offset("\"event\"")), "triggers[1].event");
        assert_eq!(json_path_at(text, 1), "");
        // Escaped quotes inside keys do not end them early.
        let text = "{\"a\\\"b\": {\"c\": 1}}";
        assert_eq!(json_path_at(text, text.find("1").unwrap()), "a\"b.c");
        // Malformed input never panics.
        for text in ["{\"a\\", "{\"é\\é", "}}]]", "{\"a\": [\"x\\\"] "] {
            json_path_at(text, text.len() + 5);
        }
        let d = parse::<Value>(DRAFT_FILE, "{\"a\": [1, \"é\\", true, None).unwrap_err();
        assert_eq!(d.code, "draft_format");
    }

    #[test]
    fn a_fault_inside_an_operation_is_placed_at_the_deepest_wrong_object() {
        let text = "{\"steps\": {\"a\": {\"op\": {\"kind\": \"return\", \"outcome\": \"x\",\n  \"output\": {\"op\": \"not\", \"valu\": {\"op\": \"literal\", \"value\": true}}},\n \"edges\": {}, \"on_error\": null}}}";
        let value: Value = serde_json::from_str(text).unwrap();
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct Steps {
            steps: BTreeMap<String, DraftStep>,
        }
        let d = parse::<Steps>(DRAFT_FILE, text, true, Some(&value)).unwrap_err();
        assert_eq!(d.path, "draft.json:steps.a.op.output", "{d:#?}");
        assert_eq!((d.line, d.column), (Some(2), Some(13)), "{d:#?}");
        assert!(d.message.contains("unknown field `valu`"), "{d:#?}");
        let mut path = vec![];
        assert!(deepest_fault(
            &value,
            "unknown field `valu`, expected `value`",
            &mut path
        ));
        assert_eq!(render(&path), "steps.a.op.output");
        assert_eq!(
            scan(text, text.len(), Some(&path)).1,
            Some(text.find("{\"op\": \"not\"").unwrap())
        );
    }
}
