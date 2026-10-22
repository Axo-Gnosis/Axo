//! Lenient JSON parsing for model output. Port of pi `utils/json-parse.ts`; the partial
//! parser replaces pi's `partial-json` dependency.

use serde_json::Value;

use crate::types::JsonMap;

/// Repair malformed JSON string literals: escape raw control characters inside strings
/// and double backslashes before invalid escape characters.
pub fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut repaired = String::with_capacity(json.len());
    let mut in_string = false;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        index += 1;
        if !in_string {
            repaired.push(c);
            in_string = c == '"';
            continue;
        }
        match c {
            '"' => {
                repaired.push(c);
                in_string = false;
            }
            '\\' => match chars.get(index) {
                None => repaired.push_str("\\\\"),
                Some('u')
                    if chars.len() >= index + 5
                        && chars[index + 1..index + 5]
                            .iter()
                            .all(char::is_ascii_hexdigit) =>
                {
                    repaired.push('\\');
                    repaired.extend(&chars[index..index + 5]);
                    index += 5;
                }
                Some(&next @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')) => {
                    repaired.push('\\');
                    repaired.push(next);
                    index += 1;
                }
                Some(_) => repaired.push_str("\\\\"),
            },
            '\u{08}' => repaired.push_str("\\b"),
            '\u{0c}' => repaired.push_str("\\f"),
            '\n' => repaired.push_str("\\n"),
            '\r' => repaired.push_str("\\r"),
            '\t' => repaired.push_str("\\t"),
            '\u{00}'..='\u{1f}' => repaired.push_str(&format!("\\u{:04x}", c as u32)),
            _ => repaired.push(c),
        }
    }
    repaired
}

/// Parse JSON, retrying once with [`repair_json`] when the first attempt fails.
pub fn parse_with_repair(json: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(json).or_else(|error| {
        let repaired = repair_json(json);
        if repaired == json {
            Err(error)
        } else {
            serde_json::from_str(&repaired)
        }
    })
}

/// Parse the complete arguments text of a finished tool call.
///
/// Never keeps a partially parsed prefix: text that is not a JSON object yields `{}` plus
/// an error describing the problem and quoting the text the model sent.
pub fn parse_tool_arguments(json: &str) -> (JsonMap, Option<String>) {
    if json.trim().is_empty() {
        return (JsonMap::new(), None);
    }
    let error = match parse_with_repair(json) {
        Ok(Value::Object(arguments)) => return (arguments, None),
        Ok(_) => "Tool arguments must be a JSON object".to_owned(),
        Err(error) => error.to_string(),
    };
    (
        JsonMap::new(),
        Some(format!("{error}\n\nReceived arguments:\n{json}")),
    )
}

/// Parse possibly incomplete JSON received while a tool call streams. Always returns an
/// object: complete JSON parses as-is; a truncated prefix keeps every complete member
/// and the partial string being written, drops a dangling key, colon, comma or
/// unfinished literal, and closes the open containers. Anything else yields `{}`.
pub fn parse_streaming(json: &str) -> JsonMap {
    if json.trim().is_empty() {
        return JsonMap::new();
    }
    let value = match parse_with_repair(json) {
        Ok(value) => Some(value),
        Err(_) => Partial {
            text: json,
            bytes: json.as_bytes(),
            at: 0,
        }
        .value()
        .ok()
        .flatten(),
    };
    match value {
        Some(Value::Object(map)) => map,
        _ => JsonMap::new(),
    }
}

/// Recursive-descent parser over a JSON prefix. `Ok(None)` means the input ended before
/// a usable value; `Err(())` means the text is not a JSON prefix.
struct Partial<'a> {
    text: &'a str,
    bytes: &'a [u8],
    at: usize,
}

impl Partial<'_> {
    fn skip_ws(&mut self) -> Option<u8> {
        while let Some(&byte) = self.bytes.get(self.at) {
            if !byte.is_ascii_whitespace() {
                return Some(byte);
            }
            self.at += 1;
        }
        None
    }

    fn value(&mut self) -> Result<Option<Value>, ()> {
        match self.skip_ws() {
            None => Ok(None),
            Some(b'{') => self.object().map(Some),
            Some(b'[') => self.array().map(Some),
            Some(b'"') => self.string().map(|text| Some(Value::String(text))),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(()),
        }
    }

    fn object(&mut self) -> Result<Value, ()> {
        self.at += 1;
        let mut map = JsonMap::new();
        loop {
            match self.skip_ws() {
                None => return Ok(Value::Object(map)),
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Value::Object(map));
                }
                Some(b',') if !map.is_empty() => {
                    self.at += 1;
                    continue;
                }
                Some(b'"') => {}
                Some(_) => return Err(()),
            }
            let key_complete = self.string_complete();
            let key = self.string()?;
            if !key_complete {
                return Ok(Value::Object(map));
            }
            match self.skip_ws() {
                None => return Ok(Value::Object(map)),
                Some(b':') => self.at += 1,
                Some(_) => return Err(()),
            }
            match self.value()? {
                Some(value) => {
                    map.insert(key, value);
                }
                None => return Ok(Value::Object(map)),
            }
            match self.skip_ws() {
                None => return Ok(Value::Object(map)),
                Some(b',' | b'}') => {}
                Some(_) => return Err(()),
            }
        }
    }

    fn array(&mut self) -> Result<Value, ()> {
        self.at += 1;
        let mut items = Vec::new();
        loop {
            match self.skip_ws() {
                None => return Ok(Value::Array(items)),
                Some(b']') => {
                    self.at += 1;
                    return Ok(Value::Array(items));
                }
                Some(b',') if !items.is_empty() => {
                    self.at += 1;
                    continue;
                }
                Some(_) => {}
            }
            match self.value()? {
                Some(value) => items.push(value),
                None => return Ok(Value::Array(items)),
            }
            match self.skip_ws() {
                None => return Ok(Value::Array(items)),
                Some(b',' | b']') => {}
                Some(_) => return Err(()),
            }
        }
    }

    /// Whether the string starting at the cursor is closed before the input ends.
    fn string_complete(&self) -> bool {
        let mut escaped = false;
        for &byte in &self.bytes[self.at + 1..] {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => return true,
                _ => {}
            }
        }
        false
    }

    /// Parse a string literal; an unterminated one yields its text so far, minus a
    /// trailing incomplete escape.
    fn string(&mut self) -> Result<String, ()> {
        let start = self.at + 1;
        let mut escaped = false;
        let mut end = None;
        for (offset, &byte) in self.bytes[start..].iter().enumerate() {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => {
                    end = Some(start + offset);
                    break;
                }
                _ => {}
            }
        }
        let body = match end {
            Some(end) => {
                self.at = end + 1;
                &self.text[start..end]
            }
            None => {
                self.at = self.bytes.len();
                let body = &self.text[start..];
                if escaped {
                    &body[..body.len() - 1]
                } else {
                    // Drop a trailing `\u` escape that lacks its 4 hex digits.
                    match body.rfind("\\u") {
                        Some(cut)
                            if body.len() - cut < 6
                                && body[..cut]
                                    .bytes()
                                    .rev()
                                    .take_while(|&b| b == b'\\')
                                    .count()
                                    % 2
                                    == 0 =>
                        {
                            &body[..cut]
                        }
                        _ => body,
                    }
                }
            }
        };
        match parse_with_repair(&format!("\"{body}\"")) {
            Ok(Value::String(text)) => Ok(text),
            _ => Err(()),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Option<Value>, ()> {
        let rest = &self.bytes[self.at..];
        let len = rest.len().min(word.len());
        if rest[..len] != word.as_bytes()[..len] {
            return Err(());
        }
        self.at += len;
        Ok((len == word.len()).then_some(value))
    }

    fn number(&mut self) -> Result<Option<Value>, ()> {
        let start = self.at;
        while let Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') = self.bytes.get(self.at) {
            self.at += 1;
        }
        let parsed = serde_json::from_str::<serde_json::Number>(&self.text[start..self.at]);
        match parsed {
            Ok(number) => Ok(Some(Value::Number(number))),
            // A number cut off by the end of input (`1.`, `-`, `2e`) is dropped.
            Err(_) if self.at == self.bytes.len() => Ok(None),
            Err(_) => Err(()),
        }
    }
}
