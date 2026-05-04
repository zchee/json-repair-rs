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
struct Parser<'a> {
    input: &'a str,
    index: usize,
    strict: bool,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, strict: bool) -> Self {
        Self {
            input,
            index: 0,
            strict,
        }
    }

    fn parse_top_level(&mut self) -> Result<Option<Value>, RepairError> {
        let mut values = Vec::new();
        while self.index < self.input.len() {
            let before = self.index;
            if let Some(value) = self.parse_value(Context::Top)?
                && !is_empty_repair(&value)
            {
                push_top_level(&mut values, value);
            }
            if self.index <= before {
                self.advance();
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
                    self.advance();
                    self.parse_object().map(Some)
                }
                '[' => {
                    self.advance();
                    self.parse_array(']').map(Some)
                }
                '(' => {
                    if context == Context::Top && !self.top_level_parenthesis_can_start_value() {
                        self.advance();
                        continue;
                    }
                    self.advance();
                    self.parse_parenthesized().map(Some)
                }
                '\\' if matches!(self.peek_next(), Some('"' | '\'' | '“')) => {
                    self.advance();
                    self.parse_quoted_string(context)
                        .map(|s| Some(Value::String(s)))
                }
                '"' | '\'' | '“' => self
                    .parse_quoted_string(context)
                    .map(|s| Some(Value::String(s))),
                '-' | '.' | '0'..='9' => {
                    if context == Context::Top
                        && self.skip_top_level_prose_prefix_before_candidate()
                    {
                        continue;
                    }
                    Ok(Some(self.parse_numberish_or_string(context)))
                }
                ch if ch.is_alphabetic() || ch == '_' => {
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
                    self.advance();
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
                    self.advance();
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
                self.advance();
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
                self.advance();
            } else if self.peek() == Some('}') {
                self.advance();
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
                    self.advance();
                    break;
                }
                Some('}') if closing == ']' => break,
                Some(')') if closing == ']' => {
                    self.advance();
                    break;
                }
                _ => {}
            }

            if self.looks_like_array_object_entry() {
                let key = self.parse_object_key()?;
                self.skip_ws_and_comments();
                if self.peek() == Some(':') {
                    self.advance();
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
                self.advance();
            }

            self.skip_ws_and_comments();
            if self.peek() == Some(',') {
                self.advance();
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
            self.advance();
        }
        match self.peek() {
            Some('"' | '\'' | '“') => self.parse_quoted_string(Context::ObjectKey),
            Some('[') => {
                self.advance();
                let key = self.parse_bare_until(&[']', ':', ',', '}'], Context::ObjectKey);
                if self.peek() == Some(']') {
                    self.advance();
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
        self.advance();
        let mut out = String::new();

        while let Some(ch) = self.peek() {
            if ch == '\\' {
                if self.peek_next() == Some(close) && self.escaped_quote_closes_string(context) {
                    self.advance();
                    self.advance();
                    break;
                }
                self.advance();
                if let Some(escaped) = self.consume_escape(close) {
                    out.push(escaped);
                } else {
                    out.push('\\');
                }
                continue;
            }

            if ch == close {
                if self.quote_closes_string(context) {
                    self.advance();
                    break;
                }
                out.push(ch);
                self.advance();
                continue;
            }

            if close_missing_before(ch, context) {
                break;
            }

            if ch == '\n' || ch == '\r' {
                out.push(ch);
                self.advance();
                continue;
            }

            out.push(ch);
            self.advance();
        }

        Ok(out.trim_end_matches('`').trim().to_string())
    }

    fn consume_escape(&mut self, quote: char) -> Option<char> {
        let escape_index = self.index;
        let ch = self.peek()?;
        self.advance();
        match ch {
            '"' | '\'' | '\\' | '/' => Some(ch),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'b' => Some('\u{0008}'),
            'f' => Some('\u{000c}'),
            'u' => self.consume_unicode_escape().or_else(|| {
                self.index = escape_index;
                None
            }),
            'x' => self
                .consume_hex_value(2)
                .and_then(char::from_u32)
                .or_else(|| {
                    self.index = escape_index;
                    None
                }),
            other if other == quote => Some(other),
            other => Some(other),
        }
    }

    fn consume_unicode_escape(&mut self) -> Option<char> {
        let value = self.consume_hex_value(4)?;
        if (0xd800..=0xdbff).contains(&value) {
            let saved = self.index;
            if self.peek() == Some('\\') && self.peek_next() == Some('u') {
                self.advance();
                self.advance();
                if let Some(trail) = self.consume_hex_value(4)
                    && (0xdc00..=0xdfff).contains(&trail)
                {
                    let codepoint = ((value - 0xd800) << 10) + (trail - 0xdc00) + 0x1_0000;
                    return char::from_u32(codepoint);
                }
            }
            self.index = saved;
        }
        char::from_u32(value)
    }

    fn consume_hex_value(&mut self, digits: usize) -> Option<u32> {
        let mut index = self.index;
        let mut value = 0_u32;
        for _ in 0..digits {
            let ch = self.char_at(index)?;
            value = value.checked_mul(16)?;
            value = value.checked_add(ch.to_digit(16)?)?;
            index = advance_index(self.input, index);
        }
        self.index = index;
        Some(value)
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

    fn skip_top_level_prose_prefix_before_candidate(&mut self) -> bool {
        let Some(candidate_index) = self.top_level_container_candidate_after_prefix() else {
            return false;
        };
        let Some(prefix) = self.input.get(self.index..candidate_index) else {
            return false;
        };
        if !prefix_looks_like_wrapper_prose(prefix) {
            return false;
        }
        self.index = candidate_index;
        true
    }

    fn top_level_container_candidate_after_prefix(&self) -> Option<usize> {
        let mut index = self.index;
        while let Some(ch) = self.char_at(index) {
            match ch {
                '{' | '[' | '`' => return Some(index),
                '"' | '\'' | '“' => return None,
                _ => index = advance_index(self.input, index),
            }
        }
        None
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
            let value = value_from_bare_token(&word);
            if matches!(&value, Value::String(string) if string.split_whitespace().count() > 1) {
                return Ok(None);
            }
            return Ok(Some(value));
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
            self.advance();
        }
        out.trim().trim_end_matches('`').trim().to_string()
    }

    fn starts_next_object_member(&self) -> bool {
        let mut i = self.index;
        if self.char_at(i).is_some_and(|ch| ch.is_whitespace()) {
            while self.char_at(i).is_some_and(|ch| ch.is_whitespace()) {
                i = advance_index(self.input, i);
            }
        } else {
            return false;
        }
        match self.char_at(i) {
            Some(open @ ('"' | '\'' | '“')) => {
                let quote = matching_quote(open);
                i = advance_index(self.input, i);
                while let Some(ch) = self.char_at(i) {
                    if ch == quote {
                        i = advance_index(self.input, i);
                        break;
                    }
                    if ch == '\n' || ch == '\r' {
                        return false;
                    }
                    i = advance_index(self.input, i);
                }
            }
            Some(ch) if bare_key_character(ch) => {
                while self.char_at(i).is_some_and(bare_key_character) {
                    i = advance_index(self.input, i);
                }
            }
            _ => return false,
        }
        while self.char_at(i).is_some_and(|ch| ch.is_whitespace()) {
            i = advance_index(self.input, i);
        }
        self.char_at(i) == Some(':')
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
        self.char_at(i) == Some(':')
    }

    fn array_value_can_follow_after_quote(&self, offset: usize) -> bool {
        if !self.has_separator_after_offset(offset) {
            return false;
        }
        let Some(i) = self.next_significant_index_after_comment(offset) else {
            return false;
        };
        self.char_at(i).is_some_and(|ch| {
            bare_value_can_start(ch) || matches!(ch, '"' | '\'' | '“' | '{' | '[' | '(')
        })
    }

    fn array_value_can_follow_after_whitespace(&self) -> bool {
        let mut i = self.index;
        while self.char_at(i).is_some_and(|ch| ch.is_whitespace()) {
            i = advance_index(self.input, i);
        }
        self.char_at(i).is_some_and(|ch| {
            bare_value_can_start(ch) || matches!(ch, '"' | '\'' | '“' | '{' | '[' | '(')
        })
    }

    fn has_separator_after_offset(&self, offset: usize) -> bool {
        let Some(index) = self.index_after_chars(offset) else {
            return false;
        };
        self.char_at(index)
            .is_some_and(|ch| ch.is_whitespace() || ch == '#' || ch == '/')
    }

    fn scan_object_key_from(&self, index: usize) -> Option<usize> {
        match self.char_at(index)? {
            '"' | '\'' | '“' => {
                let quote = matching_quote(self.char_at(index)?);
                let mut i = advance_index(self.input, index);
                while let Some(ch) = self.char_at(i) {
                    if ch == '\\' {
                        i = advance_index(self.input, i);
                        i = advance_index(self.input, i);
                        continue;
                    }
                    if ch == quote {
                        return Some(advance_index(self.input, i));
                    }
                    if ch == '\n' || ch == '\r' {
                        return None;
                    }
                    i = advance_index(self.input, i);
                }
                None
            }
            ch if bare_key_character(ch) => {
                let mut i = index;
                while self.char_at(i).is_some_and(bare_key_character) {
                    i = advance_index(self.input, i);
                }
                Some(i)
            }
            _ => None,
        }
    }

    fn looks_like_array_object_entry(&self) -> bool {
        match self.peek() {
            Some('"' | '\'' | '“' | '\\') => {}
            Some(ch) if bare_key_character(ch) => {}
            _ => return false,
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
                    self.advance();
                    self.skip_ws_and_comments();
                }
                Some('}') => {
                    self.advance();
                    break;
                }
                None => break,
                _ => {}
            }
            if matches!(self.peek(), Some('}') | None) {
                if self.peek() == Some('}') {
                    self.advance();
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
        let mut i = advance_index(self.input, self.index);
        while self.char_at(i).is_some_and(|ch| ch.is_whitespace()) {
            i = advance_index(self.input, i);
        }
        matches!(
            self.char_at(i),
            Some('{' | '[' | '(' | '"' | '\'' | '“' | '-' | '.' | '0'..='9')
        )
    }

    fn clone_probe(&self) -> Self {
        Self {
            input: self.input,
            index: self.index,
            strict: self.strict,
        }
    }

    fn skip_ws_comments_and_commas(&mut self) {
        loop {
            self.skip_ws_and_comments();
            if self.peek() == Some(',') {
                self.advance();
                continue;
            }
            break;
        }
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.advance();
            }
            if self.peek() == Some('#') {
                self.skip_line_comment();
                continue;
            }
            if self.peek() == Some('/') && self.peek_next() == Some('/') {
                self.advance();
                self.advance();
                self.skip_line_comment();
                continue;
            }
            if self.peek() == Some('/') && self.peek_next() == Some('*') {
                self.advance();
                self.advance();
                while self.index < self.input.len() {
                    if self.peek() == Some('*') && self.peek_next() == Some('/') {
                        self.advance();
                        self.advance();
                        break;
                    }
                    self.advance();
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
            self.advance();
        }
    }

    fn skip_code_fence_marker(&mut self) {
        while self.peek() == Some('`') {
            self.advance();
        }
        while self
            .peek()
            .is_some_and(|ch| !ch.is_whitespace() && ch != '`')
        {
            self.advance();
        }
        if self.peek() == Some('\r') {
            self.advance();
            if self.peek() == Some('\n') {
                self.advance();
            }
        } else if self.peek() == Some('\n') {
            self.advance();
        }
    }

    fn next_significant_after_comment(&self, offset: usize) -> Option<char> {
        let mut i = self.index_after_chars(offset)?;
        loop {
            while self.char_at(i).is_some_and(|ch| ch.is_whitespace()) {
                i = advance_index(self.input, i);
            }
            if self.char_at(i) == Some('#') {
                i = skip_line_comment_in(self.input, advance_index(self.input, i));
                continue;
            }
            if self.char_at(i) == Some('/') && char_at_next(self.input, i) == Some('/') {
                i = advance_index(self.input, i);
                i = skip_line_comment_in(self.input, advance_index(self.input, i));
                continue;
            }
            if self.char_at(i) == Some('/') && char_at_next(self.input, i) == Some('*') {
                i = advance_index(self.input, i);
                i = advance_index(self.input, i);
                while i < self.input.len() {
                    if self.char_at(i) == Some('*') && char_at_next(self.input, i) == Some('/') {
                        i = advance_index(self.input, i);
                        i = advance_index(self.input, i);
                        break;
                    }
                    i = advance_index(self.input, i);
                }
                continue;
            }
            return self.char_at(i);
        }
    }

    fn next_significant_index_after_comment(&self, offset: usize) -> Option<usize> {
        let mut i = self.index_after_chars(offset)?;
        loop {
            while self.char_at(i).is_some_and(|ch| ch.is_whitespace()) {
                i = advance_index(self.input, i);
            }
            if self.char_at(i) == Some('#') {
                i = skip_line_comment_in(self.input, advance_index(self.input, i));
                continue;
            }
            if self.char_at(i) == Some('/') && char_at_next(self.input, i) == Some('/') {
                i = advance_index(self.input, i);
                i = skip_line_comment_in(self.input, advance_index(self.input, i));
                continue;
            }
            if self.char_at(i) == Some('/') && char_at_next(self.input, i) == Some('*') {
                i = advance_index(self.input, i);
                i = advance_index(self.input, i);
                while i < self.input.len() {
                    if self.char_at(i) == Some('*') && char_at_next(self.input, i) == Some('/') {
                        i = advance_index(self.input, i);
                        i = advance_index(self.input, i);
                        break;
                    }
                    i = advance_index(self.input, i);
                }
                continue;
            }
            return self.char_at(i).map(|_| i);
        }
    }

    fn skip_ws_and_comments_from(&self, mut index: usize) -> usize {
        loop {
            while self.char_at(index).is_some_and(|ch| ch.is_whitespace()) {
                index = advance_index(self.input, index);
            }
            if self.char_at(index) == Some('#') {
                index = skip_line_comment_in(self.input, advance_index(self.input, index));
                continue;
            }
            if self.char_at(index) == Some('/') && char_at_next(self.input, index) == Some('/') {
                index = advance_index(self.input, index);
                index = skip_line_comment_in(self.input, advance_index(self.input, index));
                continue;
            }
            if self.char_at(index) == Some('/') && char_at_next(self.input, index) == Some('*') {
                index = advance_index(self.input, index);
                index = advance_index(self.input, index);
                while index < self.input.len() {
                    if self.char_at(index) == Some('*')
                        && char_at_next(self.input, index) == Some('/')
                    {
                        index = advance_index(self.input, index);
                        index = advance_index(self.input, index);
                        break;
                    }
                    index = advance_index(self.input, index);
                }
                continue;
            }
            return index;
        }
    }

    fn peek(&self) -> Option<char> {
        self.char_at(self.index)
    }

    fn peek_next(&self) -> Option<char> {
        char_at_next(self.input, self.index)
    }

    fn char_at(&self, index: usize) -> Option<char> {
        char_at(self.input, index)
    }

    fn advance(&mut self) {
        self.index = advance_index(self.input, self.index);
    }

    fn index_after_chars(&self, count: usize) -> Option<usize> {
        let mut index = self.index;
        for _ in 0..count {
            self.char_at(index)?;
            index = advance_index(self.input, index);
        }
        Some(index)
    }
}

fn char_at(input: &str, index: usize) -> Option<char> {
    input.get(index..)?.chars().next()
}

fn char_at_next(input: &str, index: usize) -> Option<char> {
    let next = advance_index(input, index);
    char_at(input, next)
}

fn advance_index(input: &str, index: usize) -> usize {
    match char_at(input, index) {
        Some(ch) => index + ch.len_utf8(),
        None => input.len(),
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

fn bare_key_character(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-')
}

fn prefix_looks_like_wrapper_prose(prefix: &str) -> bool {
    let trimmed = prefix.trim();
    !trimmed.is_empty()
        && (trimmed.contains(':')
            || trimmed.chars().any(char::is_alphabetic)
            || (trimmed.contains('-') && trimmed.chars().any(char::is_whitespace)))
}

fn skip_line_comment_in(input: &str, mut index: usize) -> usize {
    while let Some(ch) = char_at(input, index) {
        if ch == '\n' || ch == '\r' {
            break;
        }
        index = advance_index(input, index);
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
    fn repairs_unicode_without_preallocating_chars() {
        assert_eq!(
            repair_json("{city: 東京, greeting: 'こんにちは'}").expect("repair"),
            r#"{"city": "\u6771\u4eac", "greeting": "\u3053\u3093\u306b\u3061\u306f"}"#
        );
    }

    #[test]
    fn repairs_date_prefixed_prose_before_object() {
        assert_eq!(
            repair_json("2026-05-05 result: {a:1}").expect("repair"),
            r#"{"a": 1}"#
        );
    }

    #[test]
    fn repairs_non_alpha_markdown_json_fences() {
        assert_eq!(
            repair_json("```json5\n{a:1}\n```").expect("repair"),
            r#"{"a": 1}"#
        );
        assert_eq!(
            repair_json("```jsonc\n{a:1,}\n```").expect("repair"),
            r#"{"a": 1}"#
        );
    }

    #[test]
    fn repairs_unicode_bare_key_in_array_object_entry() {
        assert_eq!(
            repair_json("[東京: 1]").expect("repair"),
            r#"[{"\u6771\u4eac": 1}]"#
        );
    }

    #[test]
    fn repairs_surrogate_pair_escapes() {
        let options = RepairOptions {
            skip_json_loads: true,
            ensure_ascii: true,
            ..RepairOptions::default()
        };
        assert_eq!(
            repair_json_with_options(r#"{"emoji": "\ud83d\ude00"}"#, options).expect("repair"),
            r#"{"emoji": "\ud83d\ude00"}"#
        );

        let options = RepairOptions {
            skip_json_loads: true,
            ensure_ascii: false,
            ..RepairOptions::default()
        };
        assert_eq!(
            repair_json_with_options(r#"{"emoji": "\ud83d\ude00"}"#, options).expect("repair"),
            "{\"emoji\": \"😀\"}"
        );
    }

    #[test]
    fn preserves_lone_surrogate_escape_digits() {
        let options = RepairOptions {
            skip_json_loads: true,
            ensure_ascii: true,
            ..RepairOptions::default()
        };
        assert_eq!(
            repair_json_with_options(r#"{"lead": "\ud83d", "trail": "\ude00"}"#, options)
                .expect("repair"),
            r#"{"lead": "\\ud83d", "trail": "\\ude00"}"#
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
