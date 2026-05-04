//! Repair malformed JSON strings from LLMs, logs, and user input.
//!
//! The crate follows the broad behavior of Python's `json_repair`: valid JSON uses a
//! strict fast path, malformed input is parsed with repair heuristics, and callers can
//! receive either a repaired JSON string or a `serde_json::Value`.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde_json::{Map, Number, Value};
use thiserror::Error;

/// Errors returned by JSON repair operations.
#[derive(Debug, Error)]
pub enum RepairError {
    /// The input did not contain any recoverable JSON value.
    #[error("input did not contain a recoverable JSON value")]
    Empty,
    /// Strict mode rejected a structure that lenient repair would otherwise fix.
    #[error("strict repair rejected input: {0}")]
    Strict(String),
    /// Reading input failed.
    #[error("failed to read JSON input: {0}")]
    Io(#[from] io::Error),
    /// Serializing or parsing strict JSON failed unexpectedly.
    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// Options controlling repair and serialization behavior.
#[derive(Clone, Debug)]
pub struct RepairOptions {
    /// Skip the initial strict `serde_json` fast path and run the repair parser immediately.
    pub skip_json_loads: bool,
    /// Reject duplicate keys and multiple top-level JSON values instead of repairing them.
    pub strict: bool,
    /// Escape non-ASCII characters when serializing repaired JSON, matching Python's
    /// `json.dumps(..., ensure_ascii=True)` default.
    pub ensure_ascii: bool,
}

impl Default for RepairOptions {
    fn default() -> Self {
        Self {
            skip_json_loads: false,
            strict: false,
            ensure_ascii: true,
        }
    }
}

/// Repair a JSON string and return a serialized JSON document.
///
/// # Errors
///
/// Returns [`RepairError::Empty`] when no recoverable JSON value exists, or a strict-mode
/// error when [`RepairOptions::strict`] is enabled and incompatible malformed structures
/// are found.
///
/// # Examples
///
/// ```
/// let repaired = json_repair_rs::repair_json("{name: 'Ada', ok: tru}")?;
/// assert_eq!(repaired, r#"{"name": "Ada", "ok": true}"#);
/// # Ok::<(), json_repair_rs::RepairError>(())
/// ```
pub fn repair_json(input: &str) -> Result<String, RepairError> {
    repair_json_with_options(input, RepairOptions::default())
}

/// Repair a JSON string with explicit options and return a serialized JSON document.
///
/// # Errors
///
/// Returns an error if input cannot be repaired, strict mode rejects the input, or
/// serialization fails.
pub fn repair_json_with_options(
    input: &str,
    options: RepairOptions,
) -> Result<String, RepairError> {
    let value = loads_with_options(input, options.clone())?;
    Ok(to_json_string(&value, options.ensure_ascii))
}

/// Repair and parse a JSON string into a [`serde_json::Value`].
///
/// # Errors
///
/// Returns an error if input cannot be repaired or strict mode rejects the input.
///
/// # Examples
///
/// ```
/// let value = json_repair_rs::loads("[1, 2, ...]")?;
/// assert_eq!(value, serde_json::json!([1, 2]));
/// # Ok::<(), json_repair_rs::RepairError>(())
/// ```
pub fn loads(input: &str) -> Result<Value, RepairError> {
    loads_with_options(input, RepairOptions::default())
}

/// Repair and parse a JSON string into a [`serde_json::Value`] with explicit options.
///
/// # Errors
///
/// Returns an error if input cannot be repaired or strict mode rejects the input.
pub fn loads_with_options(input: &str, options: RepairOptions) -> Result<Value, RepairError> {
    if !options.skip_json_loads
        && !options.strict
        && let Ok(value) = serde_json::from_str::<Value>(input)
    {
        return Ok(value);
    }

    let mut parser = Parser::new(input, options.strict);
    parser.parse_top_level()?.ok_or(RepairError::Empty)
}

/// Read, repair, and serialize JSON from any reader.
///
/// # Errors
///
/// Returns an I/O error when reading fails, or a repair error when parsing fails.
pub fn repair_reader<R: Read>(
    mut reader: R,
    options: RepairOptions,
) -> Result<String, RepairError> {
    let mut input = String::new();
    reader.read_to_string(&mut input)?;
    repair_json_with_options(&input, options)
}

/// Read, repair, and parse JSON from any reader.
///
/// # Errors
///
/// Returns an I/O error when reading fails, or a repair error when parsing fails.
pub fn load_reader<R: Read>(mut reader: R, options: RepairOptions) -> Result<Value, RepairError> {
    let mut input = String::new();
    reader.read_to_string(&mut input)?;
    loads_with_options(&input, options)
}

/// Read, repair, and parse JSON from a file path.
///
/// # Errors
///
/// Returns an I/O error when the file cannot be read, or a repair error when parsing fails.
pub fn from_file(path: impl AsRef<Path>, options: RepairOptions) -> Result<Value, RepairError> {
    let file = File::open(path)?;
    load_reader(file, options)
}

/// Serialize a [`serde_json::Value`] in Python `json.dumps`-like compact form.
#[must_use]
pub fn to_json_string(value: &Value, ensure_ascii: bool) -> String {
    let mut out = String::new();
    write_value(value, ensure_ascii, &mut out);
    out
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Context {
    Top,
    ObjectKey,
    ObjectValue,
    Array,
}

#[derive(Debug)]
struct Parser {
    chars: Vec<char>,
    index: usize,
    strict: bool,
}

impl Parser {
    fn new(input: &str, strict: bool) -> Self {
        Self {
            chars: input.chars().collect(),
            index: 0,
            strict,
        }
    }

    fn parse_top_level(&mut self) -> Result<Option<Value>, RepairError> {
        let mut values = Vec::new();
        while self.index < self.chars.len() {
            let before = self.index;
            if let Some(value) = self.parse_value(Context::Top)?
                && !is_empty_repair(&value)
            {
                push_top_level(&mut values, value);
            }
            if self.index <= before {
                self.index += 1;
            }
        }

        match values.len() {
            0 => Ok(None),
            1 => Ok(values.pop()),
            _ if self.strict => Err(RepairError::Strict(
                "multiple top-level JSON values found".to_string(),
            )),
            _ => Ok(Some(Value::Array(values))),
        }
    }

    fn parse_value(&mut self, context: Context) -> Result<Option<Value>, RepairError> {
        loop {
            self.skip_ws_and_comments();
            let Some(ch) = self.peek() else {
                return Ok(None);
            };
            return match ch {
                '{' => {
                    self.index += 1;
                    self.parse_object().map(Some)
                }
                '[' => {
                    self.index += 1;
                    self.parse_array(']').map(Some)
                }
                '(' => {
                    if context == Context::Top && !self.top_level_parenthesis_can_start_value() {
                        self.index += 1;
                        continue;
                    }
                    self.index += 1;
                    self.parse_parenthesized().map(Some)
                }
                '\\' if matches!(self.peek_next(), Some('"' | '\'' | '“')) => {
                    self.index += 1;
                    self.parse_quoted_string(context)
                        .map(|s| Some(Value::String(s)))
                }
                '"' | '\'' | '“' => self
                    .parse_quoted_string(context)
                    .map(|s| Some(Value::String(s))),
                '-' | '.' | '0'..='9' => Ok(Some(self.parse_numberish_or_string(context))),
                'A'..='Z' | 'a'..='z' | '_' => {
                    let value = self.parse_bare_word_value(context)?;
                    if value.is_none() && context == Context::Top {
                        continue;
                    }
                    Ok(value)
                }
                other if other.is_alphabetic() => {
                    let value = self.parse_bare_word_value(context)?;
                    if value.is_none() && context == Context::Top {
                        continue;
                    }
                    Ok(value)
                }
                '`' => {
                    self.skip_code_fence_marker();
                    continue;
                }
                ',' | ':' if context == Context::ObjectValue => {
                    Ok(Some(Value::String(String::new())))
                }
                '}' | ']' | ')' if context != Context::Top => Ok(None),
                _ => {
                    self.index += 1;
                    if context == Context::Top {
                        continue;
                    }
                    Ok(None)
                }
            };
        }
    }

    fn parse_object(&mut self) -> Result<Value, RepairError> {
        let mut object = Map::new();
        loop {
            self.skip_ws_comments_and_commas();
            match self.peek() {
                None => break,
                Some('}') => {
                    self.index += 1;
                    break;
                }
                Some(']') | Some(')') => break,
                _ => {}
            }

            if self.peek() == Some('[') {
                break;
            }

            let key = self.parse_object_key()?;
            self.skip_ws_and_comments();
            if self.peek() == Some(':') {
                self.index += 1;
            } else if matches!(self.peek(), Some(',') | Some('}')) {
                return self.parse_set_like_object(key);
            } else if key.is_empty() && matches!(self.peek(), Some('}') | None) {
                break;
            }

            self.skip_ws_and_comments();
            let value = match self.peek() {
                Some(',') | Some('}') | Some(']') | Some(')') | None => {
                    Value::String(String::new())
                }
                _ => self
                    .parse_value(Context::ObjectValue)?
                    .unwrap_or_else(|| Value::String(String::new())),
            };

            if self.strict && object.contains_key(&key) {
                return Err(RepairError::Strict(format!("duplicate key `{key}` found")));
            }
            object.insert(key, value);

            self.skip_ws_and_comments();
            if self.peek() == Some(',') {
                self.index += 1;
            } else if self.peek() == Some('}') {
                self.index += 1;
                break;
            }
        }
        Ok(Value::Object(object))
    }

    fn parse_array(&mut self, closing: char) -> Result<Value, RepairError> {
        let mut values = Vec::new();
        loop {
            self.skip_ws_comments_and_commas();
            match self.peek() {
                None => break,
                Some(ch) if ch == closing => {
                    self.index += 1;
                    break;
                }
                Some('}') if closing == ']' => break,
                Some(')') if closing == ']' => {
                    self.index += 1;
                    break;
                }
                _ => {}
            }

            if self.looks_like_array_object_entry() {
                let key = self.parse_object_key()?;
                self.skip_ws_and_comments();
                if self.peek() == Some(':') {
                    self.index += 1;
                }
                let value = self
                    .parse_value(Context::ObjectValue)?
                    .unwrap_or_else(|| Value::String(String::new()));
                let mut object = Map::new();
                object.insert(key, value);
                values.push(Value::Object(object));
            } else if let Some(value) = self.parse_value(Context::Array)? {
                if !is_ellipsis(&value) && !is_empty_repair(&value) {
                    values.push(value);
                }
            } else if self.peek().is_some() {
                self.index += 1;
            }

            self.skip_ws_and_comments();
            if self.peek() == Some(',') {
                self.index += 1;
            }
        }
        Ok(Value::Array(values))
    }

    fn parse_parenthesized(&mut self) -> Result<Value, RepairError> {
        let value = self.parse_array(')')?;
        if let Value::Array(mut values) = value {
            if values.len() == 1 {
                Ok(values.remove(0))
            } else {
                Ok(Value::Array(values))
            }
        } else {
            Ok(value)
        }
    }

    fn parse_object_key(&mut self) -> Result<String, RepairError> {
        self.skip_ws_and_comments();
        if self.peek() == Some('\\') && matches!(self.peek_next(), Some('"' | '\'' | '“')) {
            self.index += 1;
        }
        match self.peek() {
            Some('"' | '\'' | '“') => self.parse_quoted_string(Context::ObjectKey),
            Some('[') => {
                self.index += 1;
                let key = self.parse_bare_until(&[']', ':', ',', '}'], Context::ObjectKey);
                if self.peek() == Some(']') {
                    self.index += 1;
                }
                Ok(clean_key(&key))
            }
            _ => Ok(clean_key(
                &self.parse_bare_until(&[':', ',', '}'], Context::ObjectKey),
            )),
        }
    }

    fn parse_quoted_string(&mut self, context: Context) -> Result<String, RepairError> {
        let Some(open) = self.peek() else {
            return Ok(String::new());
        };
        let close = matching_quote(open);
        self.index += 1;
        let mut out = String::new();

        while let Some(ch) = self.peek() {
            if ch == '\\' {
                if self.peek_next() == Some(close) && self.escaped_quote_closes_string(context) {
                    self.index += 2;
                    break;
                }
                self.index += 1;
                if let Some(escaped) = self.consume_escape(close) {
                    out.push(escaped);
                } else {
                    out.push('\\');
                }
                continue;
            }

            if ch == close {
                if self.quote_closes_string(context) {
                    self.index += 1;
                    break;
                }
                out.push(ch);
                self.index += 1;
                continue;
            }

            if close_missing_before(ch, context) {
                break;
            }

            if ch == '\n' || ch == '\r' {
                out.push(ch);
                self.index += 1;
                continue;
            }

            out.push(ch);
            self.index += 1;
        }

        Ok(out.trim_end_matches('`').trim().to_string())
    }

    fn consume_escape(&mut self, quote: char) -> Option<char> {
        let ch = self.peek()?;
        self.index += 1;
        match ch {
            '"' | '\'' | '\\' | '/' => Some(ch),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'b' => Some('\u{0008}'),
            'f' => Some('\u{000c}'),
            'u' => self.consume_hex_escape(4),
            'x' => self.consume_hex_escape(2),
            other if other == quote => Some(other),
            other => Some(other),
        }
    }

    fn consume_hex_escape(&mut self, digits: usize) -> Option<char> {
        if self.index + digits > self.chars.len() {
            return None;
        }
        let mut value = 0_u32;
        for offset in 0..digits {
            value = value.checked_mul(16)?;
            value = value.checked_add(self.chars[self.index + offset].to_digit(16)?)?;
        }
        self.index += digits;
        char::from_u32(value)
    }

    fn quote_closes_string(&self, context: Context) -> bool {
        self.string_closes_at_offset(context, 1)
    }

    fn escaped_quote_closes_string(&self, context: Context) -> bool {
        self.string_closes_at_offset(context, 2)
    }

    fn string_closes_at_offset(&self, context: Context, offset: usize) -> bool {
        let next = self.next_significant_after_comment(offset);
        match context {
            Context::ObjectKey => matches!(next, Some(':') | Some(',') | Some('}') | None),
            Context::ObjectValue => {
                matches!(next, Some(',') | Some('}') | Some(']') | None)
                    || self.object_member_can_follow_after_quote(offset)
            }
            Context::Array => {
                matches!(next, Some(',') | Some(']') | Some('}') | None)
                    || self.array_value_can_follow_after_quote(offset)
            }
            Context::Top => next.is_none(),
        }
    }

    fn parse_numberish_or_string(&mut self, context: Context) -> Value {
        let token = self.parse_bare_until(&[',', '}', ']', ')'], context);
        value_from_bare_token(&token)
    }

    fn parse_bare_word_value(&mut self, context: Context) -> Result<Option<Value>, RepairError> {
        if context == Context::Top {
            let word = self.parse_bare_until(&['{', '[', '(', '`'], context);
            if self.peek().is_some() && !word.trim().is_empty() {
                return Ok(None);
            }
            if word.trim().is_empty() {
                return Ok(None);
            }
            return Ok(Some(value_from_bare_token(&word)));
        }
        let token = self.parse_bare_until(&[',', '}', ']', ')'], context);
        if token.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(value_from_bare_token(&token)))
    }

    fn parse_bare_until(&mut self, stops: &[char], context: Context) -> String {
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if stops.contains(&ch) {
                break;
            }
            if context == Context::ObjectValue && self.starts_next_object_member() {
                break;
            }
            if context == Context::Array
                && ch.is_whitespace()
                && array_bare_token_can_end(&out)
                && self.array_value_can_follow_after_whitespace()
            {
                break;
            }
            if ch == '/' && self.peek_next() == Some('/') {
                break;
            }
            if ch == '/' && self.peek_next() == Some('*') {
                break;
            }
            if ch == '#' {
                break;
            }
            out.push(ch);
            self.index += 1;
        }
        out.trim().trim_end_matches('`').trim().to_string()
    }

    fn starts_next_object_member(&self) -> bool {
        let mut i = self.index;
        if self.chars.get(i).is_some_and(|ch| ch.is_whitespace()) {
            while self.chars.get(i).is_some_and(|ch| ch.is_whitespace()) {
                i += 1;
            }
        } else {
            return false;
        }
        match self.chars.get(i).copied() {
            Some('"' | '\'' | '“') => {
                let quote = matching_quote(self.chars[i]);
                i += 1;
                while let Some(ch) = self.chars.get(i).copied() {
                    if ch == quote {
                        i += 1;
                        break;
                    }
                    if ch == '\n' || ch == '\r' {
                        return false;
                    }
                    i += 1;
                }
            }
            Some(ch) if ch.is_alphanumeric() || ch == '_' || ch == '-' => {
                while self
                    .chars
                    .get(i)
                    .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-'))
                {
                    i += 1;
                }
            }
            _ => return false,
        }
        while self.chars.get(i).is_some_and(|ch| ch.is_whitespace()) {
            i += 1;
        }
        self.chars.get(i) == Some(&':')
    }
    fn object_member_can_follow_after_quote(&self, offset: usize) -> bool {
        if !self.has_separator_after_offset(offset) {
            return false;
        }
        let Some(mut i) = self.next_significant_index_after_comment(offset) else {
            return false;
        };
        i = match self.scan_object_key_from(i) {
            Some(index) => index,
            None => return false,
        };
        i = self.skip_ws_and_comments_from(i);
        self.chars.get(i) == Some(&':')
    }

    fn array_value_can_follow_after_quote(&self, offset: usize) -> bool {
        if !self.has_separator_after_offset(offset) {
            return false;
        }
        let Some(i) = self.next_significant_index_after_comment(offset) else {
            return false;
        };
        self.chars.get(i).is_some_and(|ch| {
            bare_value_can_start(*ch) || matches!(ch, '"' | '\'' | '“' | '{' | '[' | '(')
        })
    }

    fn array_value_can_follow_after_whitespace(&self) -> bool {
        let mut i = self.index;
        while self.chars.get(i).is_some_and(|ch| ch.is_whitespace()) {
            i += 1;
        }
        self.chars.get(i).is_some_and(|ch| {
            bare_value_can_start(*ch) || matches!(ch, '"' | '\'' | '“' | '{' | '[' | '(')
        })
    }

    fn has_separator_after_offset(&self, offset: usize) -> bool {
        let index = self.index + offset;
        self.chars
            .get(index)
            .is_some_and(|ch| ch.is_whitespace() || *ch == '#' || *ch == '/')
    }

    fn scan_object_key_from(&self, index: usize) -> Option<usize> {
        match self.chars.get(index).copied()? {
            '"' | '\'' | '“' => {
                let quote = matching_quote(self.chars[index]);
                let mut i = index + 1;
                while let Some(ch) = self.chars.get(i).copied() {
                    if ch == '\\' {
                        i = (i + 2).min(self.chars.len());
                        continue;
                    }
                    if ch == quote {
                        return Some(i + 1);
                    }
                    if ch == '\n' || ch == '\r' {
                        return None;
                    }
                    i += 1;
                }
                None
            }
            ch if ch.is_alphanumeric() || ch == '_' || ch == '-' => {
                let mut i = index;
                while self
                    .chars
                    .get(i)
                    .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-'))
                {
                    i += 1;
                }
                Some(i)
            }
            _ => None,
        }
    }

    fn looks_like_array_object_entry(&self) -> bool {
        if !matches!(
            self.peek(),
            Some('"' | '\'' | '“') | Some('\\') | Some('A'..='Z' | 'a'..='z' | '_' | '-' | '0'..='9')
        ) {
            return false;
        }
        let mut probe = self.clone_probe();
        let key = match probe.parse_object_key() {
            Ok(key) => key,
            Err(_) => return false,
        };
        if key.is_empty() {
            return false;
        }
        probe.skip_ws_and_comments();
        probe.peek() == Some(':')
    }

    fn parse_set_like_object(&mut self, first_key: String) -> Result<Value, RepairError> {
        let mut values = vec![Value::String(first_key)];
        loop {
            self.skip_ws_and_comments();
            match self.peek() {
                Some(',') => {
                    self.index += 1;
                    self.skip_ws_and_comments();
                }
                Some('}') => {
                    self.index += 1;
                    break;
                }
                None => break,
                _ => {}
            }
            if matches!(self.peek(), Some('}') | None) {
                if self.peek() == Some('}') {
                    self.index += 1;
                }
                break;
            }
            let item = self.parse_object_key()?;
            if !item.is_empty() {
                values.push(Value::String(item));
            }
        }
        Ok(Value::Array(values))
    }

    fn top_level_parenthesis_can_start_value(&self) -> bool {
        let mut i = self.index + 1;
        while self.chars.get(i).is_some_and(|ch| ch.is_whitespace()) {
            i += 1;
        }
        matches!(
            self.chars.get(i),
            Some('{' | '[' | '(' | '"' | '\'' | '“' | '-' | '.' | '0'..='9')
        )
    }

    fn clone_probe(&self) -> Self {
        Self {
            chars: self.chars.clone(),
            index: self.index,
            strict: self.strict,
        }
    }

    fn skip_ws_comments_and_commas(&mut self) {
        loop {
            self.skip_ws_and_comments();
            if self.peek() == Some(',') {
                self.index += 1;
                continue;
            }
            break;
        }
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.index += 1;
            }
            if self.peek() == Some('#') {
                self.skip_line_comment();
                continue;
            }
            if self.peek() == Some('/') && self.peek_next() == Some('/') {
                self.index += 2;
                self.skip_line_comment();
                continue;
            }
            if self.peek() == Some('/') && self.peek_next() == Some('*') {
                self.index += 2;
                while self.index < self.chars.len() {
                    if self.peek() == Some('*') && self.peek_next() == Some('/') {
                        self.index += 2;
                        break;
                    }
                    self.index += 1;
                }
                continue;
            }
            break;
        }
    }

    fn skip_line_comment(&mut self) {
        while let Some(ch) = self.peek() {
            if matches!(ch, '\n' | '\r' | '}' | ']') {
                break;
            }
            self.index += 1;
        }
    }

    fn skip_code_fence_marker(&mut self) {
        while self.peek() == Some('`') {
            self.index += 1;
        }
        while self.peek().is_some_and(|ch| ch.is_alphabetic()) {
            self.index += 1;
        }
    }

    fn next_significant_after_comment(&self, offset: usize) -> Option<char> {
        let mut i = self.index + offset;
        loop {
            while self.chars.get(i).is_some_and(|ch| ch.is_whitespace()) {
                i += 1;
            }
            if self.chars.get(i) == Some(&'#') {
                i = skip_line_comment_in(&self.chars, i + 1);
                continue;
            }
            if self.chars.get(i) == Some(&'/') && self.chars.get(i + 1) == Some(&'/') {
                i = skip_line_comment_in(&self.chars, i + 2);
                continue;
            }
            if self.chars.get(i) == Some(&'/') && self.chars.get(i + 1) == Some(&'*') {
                i += 2;
                while i < self.chars.len() {
                    if self.chars.get(i) == Some(&'*') && self.chars.get(i + 1) == Some(&'/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            return self.chars.get(i).copied();
        }
    }

    fn next_significant_index_after_comment(&self, offset: usize) -> Option<usize> {
        let mut i = self.index + offset;
        loop {
            while self.chars.get(i).is_some_and(|ch| ch.is_whitespace()) {
                i += 1;
            }
            if self.chars.get(i) == Some(&'#') {
                i = skip_line_comment_in(&self.chars, i + 1);
                continue;
            }
            if self.chars.get(i) == Some(&'/') && self.chars.get(i + 1) == Some(&'/') {
                i = skip_line_comment_in(&self.chars, i + 2);
                continue;
            }
            if self.chars.get(i) == Some(&'/') && self.chars.get(i + 1) == Some(&'*') {
                i += 2;
                while i < self.chars.len() {
                    if self.chars.get(i) == Some(&'*') && self.chars.get(i + 1) == Some(&'/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            return self.chars.get(i).map(|_| i);
        }
    }

    fn skip_ws_and_comments_from(&self, mut index: usize) -> usize {
        loop {
            while self.chars.get(index).is_some_and(|ch| ch.is_whitespace()) {
                index += 1;
            }
            if self.chars.get(index) == Some(&'#') {
                index = skip_line_comment_in(&self.chars, index + 1);
                continue;
            }
            if self.chars.get(index) == Some(&'/') && self.chars.get(index + 1) == Some(&'/') {
                index = skip_line_comment_in(&self.chars, index + 2);
                continue;
            }
            if self.chars.get(index) == Some(&'/') && self.chars.get(index + 1) == Some(&'*') {
                index += 2;
                while index < self.chars.len() {
                    if self.chars.get(index) == Some(&'*')
                        && self.chars.get(index + 1) == Some(&'/')
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
                continue;
            }
            return index;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.index + 1).copied()
    }
}

fn array_bare_token_can_end(token: &str) -> bool {
    let token = token.trim();
    if token.is_empty() {
        return false;
    }
    let lower = token.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "true" | "tru" | "t" | "false" | "fals" | "f" | "null" | "none" | "nul" | "n"
    ) || token
        .chars()
        .next()
        .is_some_and(|ch| matches!(ch, '-' | '.' | '0'..='9'))
}

fn bare_value_can_start(ch: char) -> bool {
    matches!(ch, '-' | '.' | '0'..='9' | '_' | 'A'..='Z' | 'a'..='z') || ch.is_alphabetic()
}

fn skip_line_comment_in(chars: &[char], mut index: usize) -> usize {
    while let Some(ch) = chars.get(index) {
        if *ch == '\n' || *ch == '\r' {
            break;
        }
        index += 1;
    }
    index
}

fn push_top_level(values: &mut Vec<Value>, value: Value) {
    if let (Some(Value::Object(previous)), Value::Object(next)) = (values.last_mut(), &value)
        && (previous.keys().any(|key| next.contains_key(key))
            || previous.is_empty()
            || next.is_empty())
    {
        for (key, item) in next {
            previous.insert(key.clone(), item.clone());
        }
        return;
    }
    if let Some(previous) = values.last()
        && previous == &value
    {
        let _ = values.pop();
    }
    values.push(value);
}

fn value_from_bare_token(token: &str) -> Value {
    let cleaned = token
        .trim()
        .trim_matches('`')
        .trim_end_matches('.')
        .trim()
        .to_string();
    let lower = cleaned.to_ascii_lowercase();
    match lower.as_str() {
        "true" | "tru" | "t" => Value::Bool(true),
        "false" | "fals" | "f" => Value::Bool(false),
        "null" | "none" | "nul" | "n" => Value::Null,
        "..." => Value::String("...".to_string()),
        _ => parse_number_value(&cleaned).unwrap_or_else(|| Value::String(cleaned)),
    }
}

fn parse_number_value(token: &str) -> Option<Value> {
    let normalized = token.replace('_', "");
    if normalized.is_empty()
        || normalized.contains('/')
        || normalized.chars().skip(1).any(|ch| ch == '-')
        || normalized
            .chars()
            .any(|ch| ch.is_alphabetic() && !matches!(ch, 'e' | 'E'))
        || normalized.matches('.').count() > 1
    {
        return None;
    }

    let normalized = if let Some(rest) = normalized.strip_prefix('.') {
        format!("0.{rest}")
    } else if normalized.ends_with('e') || normalized.ends_with('E') {
        normalized[..normalized.len() - 1].to_string()
    } else {
        normalized
    };

    if normalized.contains('.') || normalized.contains('e') || normalized.contains('E') {
        normalized
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number)
    } else {
        normalized
            .parse::<i64>()
            .ok()
            .map(Number::from)
            .map(Value::Number)
            .or_else(|| {
                normalized
                    .parse::<u64>()
                    .ok()
                    .map(Number::from)
                    .map(Value::Number)
            })
    }
}

fn clean_key(key: &str) -> String {
    key.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('“')
        .trim_matches('”')
        .trim()
        .to_string()
}

fn matching_quote(open: char) -> char {
    match open {
        '“' => '”',
        other => other,
    }
}

fn close_missing_before(ch: char, context: Context) -> bool {
    match context {
        Context::ObjectKey => matches!(ch, ':' | '}' | '\n' | '\r'),
        Context::ObjectValue => matches!(ch, '}' | ']'),
        Context::Array => matches!(ch, ']'),
        Context::Top => false,
    }
}

fn is_empty_repair(value: &Value) -> bool {
    matches!(value, Value::String(s) if s.is_empty())
}

fn is_ellipsis(value: &Value) -> bool {
    matches!(value, Value::String(s) if s == "...")
}

fn write_value(value: &Value, ensure_ascii: bool, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(string) => write_string(string, ensure_ascii, out),
        Value::Array(values) => {
            out.push('[');
            for (index, item) in values.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(item, ensure_ascii, out);
            }
            out.push(']');
        }
        Value::Object(object) => {
            out.push('{');
            for (index, (key, item)) in object.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_string(key, ensure_ascii, out);
                out.push_str(": ");
                write_value(item, ensure_ascii, out);
            }
            out.push('}');
        }
    }
}

fn write_string(input: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            ch if ch < ' ' => push_unicode_escape(ch as u32, out),
            ch if ensure_ascii && !ch.is_ascii() => push_ascii_escape(ch, out),
            ch => out.push(ch),
        }
    }
    out.push('"');
}

fn push_ascii_escape(ch: char, out: &mut String) {
    let code = ch as u32;
    if code <= 0xffff {
        push_unicode_escape(code, out);
    } else {
        let adjusted = code - 0x1_0000;
        let high = 0xd800 + ((adjusted >> 10) & 0x3ff);
        let low = 0xdc00 + (adjusted & 0x3ff);
        push_unicode_escape(high, out);
        push_unicode_escape(low, out);
    }
}

fn push_unicode_escape(code: u32, out: &mut String) {
    out.push_str("\\u");
    for shift in [12, 8, 4, 0] {
        let digit = ((code >> shift) & 0xf) as u8;
        out.push(char::from(if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        }));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        RepairOptions, from_file, load_reader, loads, repair_json, repair_json_with_options,
        repair_reader, to_json_string,
    };

    #[test]
    fn repairs_valid_json_with_python_style_spacing() {
        assert_eq!(
            repair_json(r#"{"name":"John","age":30,"city":"New York"}"#).expect("repair"),
            r#"{"name": "John", "age": 30, "city": "New York"}"#
        );
        assert_eq!(
            repair_json(r#"{"key":"value☺"}"#).expect("repair"),
            r#"{"key": "value\u263a"}"#
        );
    }

    #[test]
    fn repairs_objects_with_bare_keys_values_and_missing_delimiters() {
        assert_eq!(
            loads("{name: John, age: 30, city: New York").expect("loads"),
            json!({"name": "John", "age": 30, "city": "New York"})
        );
        assert_eq!(
            repair_json("{'key': 'string', 'key2': false, \"key3\": null, \"key4\": unquoted}")
                .expect("repair"),
            r#"{"key": "string", "key2": false, "key3": null, "key4": "unquoted"}"#
        );
        assert_eq!(
            repair_json(r#"{"key": , "key2": "value2"}"#).expect("repair"),
            r#"{"key": "", "key2": "value2"}"#
        );
    }

    #[test]
    fn repairs_arrays_comments_and_ellipsis() {
        assert_eq!(repair_json("[1, 2, 3,").expect("repair"), "[1, 2, 3]");
        assert_eq!(repair_json("[1, 2, 3, ...]").expect("repair"), "[1, 2, 3]");
        assert_eq!(
            repair_json(r#"{ "key": { "key2": "value2" // comment }, "key3": "value3" }"#)
                .expect("repair"),
            r#"{"key": {"key2": "value2"}, "key3": "value3"}"#
        );
        assert_eq!(
            repair_json(r#"[ "key":"value" ]"#).expect("repair"),
            r#"[{"key": "value"}]"#
        );
    }

    #[test]
    fn repairs_arrays_with_missing_separators() {
        assert_eq!(loads("[1 2 3]").expect("loads"), json!([1, 2, 3]));
        assert_eq!(
            loads("[true false null]").expect("loads"),
            json!([true, false, null])
        );
        assert_eq!(
            loads(r#"["a" "b" "c"]"#).expect("loads"),
            json!(["a", "b", "c"])
        );
        assert_eq!(loads("[1 foo bar]").expect("loads"), json!([1, "foo bar"]));
        assert_eq!(loads("[New York]").expect("loads"), json!(["New York"]));
        assert_eq!(
            loads("[10-20 abc]").expect("loads"),
            json!(["10-20", "abc"])
        );
    }

    #[test]
    fn repairs_quoted_object_values_with_missing_separator() {
        assert_eq!(
            loads(r#"{"a": "x" "b": "y"}"#).expect("loads"),
            json!({"a": "x", "b": "y"})
        );
    }

    #[test]
    fn repairs_numbers_and_python_literals() {
        assert_eq!(
            loads(r#"{"value": 82_461_110}"#).expect("loads"),
            json!({"value": 82461110})
        );
        assert_eq!(
            loads(r#"{"key": .25}"#).expect("loads"),
            json!({"key": 0.25})
        );
        assert_eq!(
            loads(r#"{"key": 1/3}"#).expect("loads"),
            json!({"key": "1/3"})
        );
        assert_eq!(
            loads(r#"{"key": True, "n": None}"#).expect("loads"),
            json!({"key": true, "n": null})
        );
    }

    #[test]
    fn repairs_parenthesized_values_and_fenced_json() {
        assert_eq!(loads("(1, 2)").expect("loads"), json!([1, 2]));
        assert_eq!(loads("(1)").expect("loads"), json!(1));
        assert_eq!(
            repair_json("Based on this: ```json { 'a': 'b' } ```").expect("repair"),
            r#"{"a": "b"}"#
        );
    }

    #[test]
    fn supports_options_for_ascii_and_strict_duplicates() {
        let options = RepairOptions {
            ensure_ascii: false,
            ..RepairOptions::default()
        };
        assert_eq!(
            repair_json_with_options("{'test_chinese_ascii':'统一码'}", options).expect("repair"),
            r#"{"test_chinese_ascii": "统一码"}"#
        );

        let strict = RepairOptions {
            strict: true,
            skip_json_loads: false,
            ensure_ascii: true,
        };
        assert!(repair_json_with_options(r#"{"a": 1, "a": 2}"#, strict).is_err());
    }

    #[test]
    fn treats_non_ascii_bare_tokens_as_strings_without_panicking() {
        assert_eq!(
            loads("{value: 统一码}").expect("loads"),
            json!({"value": "统一码"})
        );
    }

    #[test]
    fn repairs_from_readers_and_files() {
        let options = RepairOptions::default();
        assert_eq!(
            repair_reader("{name: Ada}".as_bytes(), options.clone()).expect("repair reader"),
            r#"{"name": "Ada"}"#
        );
        assert_eq!(
            load_reader("{ok: true}".as_bytes(), options.clone()).expect("load reader"),
            json!({"ok": true})
        );

        let path = std::env::temp_dir().join(format!(
            "json_repair_rs_{}_{}.json",
            std::process::id(),
            "from_file"
        ));
        std::fs::write(&path, "{count: 1,}").expect("write fixture");
        let value = from_file(&path, options).expect("from file");
        std::fs::remove_file(&path).expect("remove fixture");
        assert_eq!(value, json!({"count": 1}));
    }

    #[test]
    fn serializes_values_with_ascii_control() {
        assert_eq!(
            to_json_string(&json!({"emoji": "☺"}), true),
            r#"{"emoji": "\u263a"}"#
        );
        assert_eq!(
            to_json_string(&json!({"emoji": "☺"}), false),
            r#"{"emoji": "☺"}"#
        );
    }
}
