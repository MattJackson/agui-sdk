//! Reads the `.proto` files and `proto-freeze.txt`, and emits
//! `src/proto/fields.rs`.
//!
//! The reader accepts the proto3 subset the AG-UI files use (messages,
//! nested messages, enums, `oneof`, `optional`/`repeated` labels, and the
//! well-known `google.protobuf.Value`/`Struct`) and refuses anything else.
//! Every field and enum value must carry the number `proto-freeze.txt`
//! assigns to its slot: the freeze file is the authority on wire numbers, the
//! `.proto` files on types and labels. A frozen slot of a declared message
//! or enum that no longer has a field or value is recorded as reserved (and
//! must match the `.proto`'s `reserved` statements), so a decoder can tell a
//! retired slot from an unknown one.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::{header, screaming_snake, string_literal};

/// Largest legal protobuf field number (2^29 - 1).
const MAX_FIELD_NUMBER: u32 = 536_870_911;
/// Field numbers protobuf reserves for its own implementation.
const RESERVED_FIELD_NUMBERS: std::ops::RangeInclusive<u32> = 19_000..=19_999;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoField {
    pub name: String,
    pub number: u32,
    pub label: Label,
    pub type_name: String,
    pub oneof: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Implicit,
    Optional,
    Repeated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoMessage {
    /// Dotted name relative to the package (`ToolCall.Function`).
    pub name: String,
    pub fields: Vec<ProtoField>,
    /// Numbers listed in `reserved` statements.
    pub reserved: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoEnum {
    pub name: String,
    pub values: Vec<(String, i32)>,
    /// Numbers listed in `reserved` statements.
    pub reserved: Vec<i64>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProtoSet {
    pub package: Option<String>,
    pub messages: Vec<ProtoMessage>,
    pub enums: Vec<ProtoEnum>,
}

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    Int(String),
    Str(String),
    Punct(char),
}

fn lex(src: &str) -> Result<Vec<(Token, usize)>, String> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(format!("line {line}: unterminated comment"));
                }
                i += 2;
            }
            b'"' | b'\'' => {
                let start = i + 1;
                i += 1;
                while i < bytes.len() && bytes[i] != b {
                    if bytes[i] == b'\\' || bytes[i] == b'\n' {
                        return Err(format!("line {line}: escapes in strings are not supported"));
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(format!("line {line}: unterminated string"));
                }
                out.push((Token::Str(src[start..i].to_owned()), line));
                i += 1;
            }
            b'0'..=b'9' | b'-' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                out.push((Token::Int(src[start..i].to_owned()), line));
            }
            b if b.is_ascii_alphabetic() || b == b'_' || b == b'.' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
                {
                    i += 1;
                }
                out.push((Token::Ident(src[start..i].to_owned()), line));
            }
            b'{' | b'}' | b';' | b'=' | b'(' | b')' | b'[' | b']' | b'<' | b'>' | b',' => {
                out.push((Token::Punct(char::from(b)), line));
                i += 1;
            }
            _ => {
                return Err(format!(
                    "line {line}: unexpected character {:?}",
                    char::from(b)
                ))
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- parser

struct Parser<'a> {
    file: &'a str,
    tokens: Vec<(Token, usize)>,
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, message: impl std::fmt::Display) -> String {
        let line = self
            .tokens
            .get(self.pos)
            .or_else(|| self.tokens.last())
            .map_or(0, |t| t.1);
        format!("{}:{line}: {message}", self.file)
    }

    fn next(&mut self) -> Result<Token, String> {
        let t = self
            .tokens
            .get(self.pos)
            .map(|t| t.0.clone())
            .ok_or_else(|| self.err("unexpected end of file"))?;
        self.pos += 1;
        Ok(t)
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|t| &t.0)
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.next()? {
            Token::Ident(s) => Ok(s),
            t => Err(self.err(format!("expected an identifier, found {t:?}"))),
        }
    }

    fn punct(&mut self, c: char) -> Result<(), String> {
        match self.next()? {
            Token::Punct(p) if p == c => Ok(()),
            t => Err(self.err(format!("expected {c:?}, found {t:?}"))),
        }
    }

    fn int(&mut self) -> Result<i64, String> {
        match self.next()? {
            Token::Int(s) => s.parse().map_err(|_| self.err(format!("bad number {s}"))),
            t => Err(self.err(format!("expected a number, found {t:?}"))),
        }
    }

    /// The body of a `reserved` statement (the keyword already consumed):
    /// a comma-separated list of numbers. Ranges and names are refused.
    fn reserved(&mut self, into: &mut Vec<i64>) -> Result<(), String> {
        loop {
            match self.next()? {
                Token::Int(s) => {
                    into.push(s.parse().map_err(|_| self.err(format!("bad number {s}")))?)
                }
                Token::Str(_) => return Err(self.err("reserved names are not supported")),
                t => return Err(self.err(format!("expected a reserved number, found {t:?}"))),
            }
            match self.next()? {
                Token::Punct(',') => {}
                Token::Punct(';') => return Ok(()),
                Token::Ident(w) if w == "to" => {
                    return Err(self.err("reserved ranges are not supported"))
                }
                t => return Err(self.err(format!("expected ',' or ';', found {t:?}"))),
            }
        }
    }

    fn file(&mut self, set: &mut ProtoSet) -> Result<(), String> {
        let mut saw_syntax = false;
        while let Some(token) = self.peek().cloned() {
            let Token::Ident(word) = token else {
                return Err(self.err("expected a top-level declaration"));
            };
            if !saw_syntax && word != "syntax" {
                return Err(self.err("the file must start with syntax = \"proto3\";"));
            }
            match word.as_str() {
                "syntax" if saw_syntax => return Err(self.err("second syntax statement")),
                "syntax" => {
                    saw_syntax = true;
                    self.pos += 1;
                    self.punct('=')?;
                    match self.next()? {
                        Token::Str(s) if s == "proto3" => {}
                        t => return Err(self.err(format!("only proto3 is supported, found {t:?}"))),
                    }
                    self.punct(';')?;
                }
                "package" => {
                    self.pos += 1;
                    let name = self.ident()?;
                    self.punct(';')?;
                    match &set.package {
                        Some(p) if *p != name => {
                            return Err(self.err(format!("second package {name} (first {p})")))
                        }
                        _ => set.package = Some(name),
                    }
                }
                "import" => {
                    self.pos += 1;
                    match self.next()? {
                        Token::Str(_) => {}
                        t => return Err(self.err(format!("expected an import path, found {t:?}"))),
                    }
                    self.punct(';')?;
                }
                "option" => {
                    // File options (language-specific namespaces) do not
                    // affect the wire.
                    self.pos += 1;
                    self.ident()?;
                    self.punct('=')?;
                    self.next()?;
                    self.punct(';')?;
                }
                "message" => {
                    self.pos += 1;
                    self.message("", set)?;
                }
                "enum" => {
                    self.pos += 1;
                    self.enumeration("", set)?;
                }
                other => return Err(self.err(format!("unsupported declaration {other:?}"))),
            }
        }
        if !saw_syntax {
            return Err(self.err("missing syntax = \"proto3\";"));
        }
        Ok(())
    }

    fn message(&mut self, scope: &str, set: &mut ProtoSet) -> Result<(), String> {
        let short = self.ident()?;
        let name = if scope.is_empty() {
            short
        } else {
            format!("{scope}.{short}")
        };
        self.punct('{')?;
        let mut fields = Vec::new();
        let mut reserved = Vec::new();
        loop {
            match self.peek().cloned() {
                Some(Token::Punct('}')) => {
                    self.pos += 1;
                    break;
                }
                Some(Token::Ident(word)) => match word.as_str() {
                    "message" => {
                        self.pos += 1;
                        self.message(&name, set)?;
                    }
                    "enum" => {
                        self.pos += 1;
                        self.enumeration(&name, set)?;
                    }
                    "oneof" => {
                        self.pos += 1;
                        let group = self.ident()?;
                        self.punct('{')?;
                        while self.peek() != Some(&Token::Punct('}')) {
                            let mut field = self.field(Label::Implicit)?;
                            field.oneof = Some(group.clone());
                            fields.push(field);
                        }
                        self.punct('}')?;
                    }
                    "optional" => {
                        self.pos += 1;
                        fields.push(self.field(Label::Optional)?);
                    }
                    "repeated" => {
                        self.pos += 1;
                        fields.push(self.field(Label::Repeated)?);
                    }
                    "reserved" => {
                        self.pos += 1;
                        self.reserved(&mut reserved)?;
                    }
                    "extensions" | "map" | "option" | "group" | "required" => {
                        return Err(self.err(format!("unsupported message element {word:?}")))
                    }
                    _ => fields.push(self.field(Label::Implicit)?),
                },
                _ => return Err(self.err("expected a message element")),
            }
        }
        if set.messages.iter().any(|m| m.name == name) {
            return Err(self.err(format!("message {name} declared twice")));
        }
        set.messages.push(ProtoMessage {
            name,
            fields,
            reserved,
        });
        Ok(())
    }

    fn field(&mut self, label: Label) -> Result<ProtoField, String> {
        let type_name = self.ident()?;
        let name = self.ident()?;
        self.punct('=')?;
        let number = self.int()?;
        if self.peek() == Some(&Token::Punct('[')) {
            return Err(self.err("field options are not supported"));
        }
        self.punct(';')?;
        let number = u32::try_from(number)
            .ok()
            .filter(|n| (1..=MAX_FIELD_NUMBER).contains(n) && !RESERVED_FIELD_NUMBERS.contains(n))
            .ok_or_else(|| self.err(format!("field number {number} out of range")))?;
        Ok(ProtoField {
            name,
            number,
            label,
            type_name,
            oneof: None,
        })
    }

    fn enumeration(&mut self, scope: &str, set: &mut ProtoSet) -> Result<(), String> {
        let short = self.ident()?;
        let name = if scope.is_empty() {
            short
        } else {
            format!("{scope}.{short}")
        };
        self.punct('{')?;
        let mut values = Vec::new();
        let mut reserved = Vec::new();
        while self.peek() != Some(&Token::Punct('}')) {
            let value = self.ident()?;
            if value == "reserved" && self.peek() != Some(&Token::Punct('=')) {
                self.reserved(&mut reserved)?;
                continue;
            }
            self.punct('=')?;
            let number = self.int()?;
            self.punct(';')?;
            let number = i32::try_from(number).map_err(|_| self.err("enum value out of range"))?;
            values.push((value, number));
        }
        self.punct('}')?;
        if values.first().map(|v| v.1) != Some(0) {
            return Err(self.err(format!("proto3 enum {name} must start with 0")));
        }
        set.enums.push(ProtoEnum {
            name,
            values,
            reserved,
        });
        Ok(())
    }
}

/// Parses `(file name, source)` pairs into one set (one package).
pub fn parse(files: &[(String, String)]) -> Result<ProtoSet, String> {
    let mut set = ProtoSet::default();
    for (file, src) in files {
        let mut parser = Parser {
            file,
            tokens: lex(src).map_err(|e| format!("{file}: {e}"))?,
            pos: 0,
        };
        parser.file(&mut set)?;
    }
    Ok(set)
}

/// Parses `proto-freeze.txt`: `Slot.path = number` lines, `#` comments.
pub fn parse_freeze(text: &str) -> Result<BTreeMap<String, i64>, String> {
    let mut out = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("proto-freeze.txt:{}: expected `slot = number`", i + 1))?;
        let number: i64 = value
            .trim()
            .parse()
            .map_err(|_| format!("proto-freeze.txt:{}: bad number", i + 1))?;
        if out.insert(key.trim().to_owned(), number).is_some() {
            return Err(format!("proto-freeze.txt:{}: slot listed twice", i + 1));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- model

const SCALARS: &[(&str, &str)] = &[
    ("bool", "Bool"),
    ("bytes", "Bytes"),
    ("double", "Double"),
    ("float", "Float"),
    ("int32", "Int32"),
    ("int64", "Int64"),
    ("string", "String"),
    ("uint32", "Uint32"),
    ("uint64", "Uint64"),
    ("google.protobuf.Struct", "Struct"),
    ("google.protobuf.Value", "Value"),
];

fn json_name(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut upper = false;
    for c in snake.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Resolves a type name used inside `scope` (protobuf scoping: innermost
/// enclosing scope first).
fn resolve(type_name: &str, scope: &str, set: &ProtoSet) -> Result<String, String> {
    if let Some((_, kind)) = SCALARS.iter().find(|(n, _)| *n == type_name) {
        return Ok((*kind).to_owned());
    }
    let package_prefix = set.package.as_deref().map(|p| format!(".{p}."));
    let bare = match &package_prefix {
        Some(prefix) if type_name.starts_with(prefix.as_str()) => &type_name[prefix.len()..],
        _ => type_name,
    };
    let mut scope = scope.to_owned();
    loop {
        let candidate = if scope.is_empty() {
            bare.to_owned()
        } else {
            format!("{scope}.{bare}")
        };
        if set.messages.iter().any(|m| m.name == candidate) {
            return Ok(format!("Message({})", screaming_snake(&candidate)));
        }
        if set.enums.iter().any(|e| e.name == candidate) {
            return Ok(format!("Enum({})", screaming_snake(&candidate)));
        }
        if scope.is_empty() {
            return Err(format!("unknown type {type_name:?}"));
        }
        scope = scope
            .rsplit_once('.')
            .map_or(String::new(), |(s, _)| s.to_owned());
    }
}

/// The declaration a freeze key's number belongs to: the key without its
/// last segment and without trailing lowercase (oneof) segments, the same
/// grouping upstream's generator allocates numbers by.
fn freeze_group(key: &str) -> &str {
    let mut group = key.rsplit_once('.').map_or("", |(g, _)| g);
    while let Some((parent, last)) = group.rsplit_once('.') {
        if !last.starts_with(|c: char| c.is_ascii_lowercase()) {
            break;
        }
        group = parent;
    }
    group
}

/// Renders `src/proto/fields.rs` (before rustfmt).
pub fn render(
    set: &ProtoSet,
    freeze: &BTreeMap<String, i64>,
    commit: &str,
) -> Result<String, String> {
    // Check every slot against the freeze, and collect retired slots.
    let mut used: BTreeSet<&str> = BTreeSet::new();
    let mut slot_check = |key: String, number: i64| -> Result<(), String> {
        match freeze.get_key_value(&key) {
            Some((k, &frozen)) if frozen == number => {
                used.insert(k.as_str());
                Ok(())
            }
            Some((_, &frozen)) => Err(format!(
                "{key} is numbered {number} in the .proto but {frozen} in proto-freeze.txt"
            )),
            None => Err(format!("{key} = {number} is not in proto-freeze.txt")),
        }
    };
    for m in &set.messages {
        for f in &m.fields {
            let key = match &f.oneof {
                Some(group) => format!("{}.{group}.{}", m.name, f.name),
                None => format!("{}.{}", m.name, f.name),
            };
            slot_check(key, i64::from(f.number))?;
        }
    }
    for e in &set.enums {
        for (value, number) in &e.values {
            slot_check(format!("{}.{value}", e.name), i64::from(*number))?;
        }
    }
    // A frozen slot with no field left is retired: file it under the
    // declaration whose number space it belongs to (the key minus its last
    // segment and any lowercase oneof segments, as upstream's freeze groups
    // keys). A slot whose declaration is gone has no table entry to go in.
    let owners: BTreeMap<&str, Vec<i64>> = set
        .messages
        .iter()
        .map(|m| {
            let live = m.fields.iter().map(|f| i64::from(f.number)).collect();
            (m.name.as_str(), live)
        })
        .chain(set.enums.iter().map(|e| {
            let live = e.values.iter().map(|v| i64::from(v.1)).collect();
            (e.name.as_str(), live)
        }))
        .collect();
    let mut retired_by_owner: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for (key, &number) in freeze {
        if used.contains(key.as_str()) {
            continue;
        }
        let Some((owner, live)) = owners.get_key_value(freeze_group(key)) else {
            continue;
        };
        if live.contains(&number) {
            return Err(format!(
                "{key} = {number} is retired, but {owner} still uses {number}"
            ));
        }
        retired_by_owner.entry(owner).or_default().push(number);
    }
    for list in retired_by_owner.values_mut() {
        list.sort_unstable();
        list.dedup();
    }
    let retired =
        |owner: &str| -> Vec<i64> { retired_by_owner.get(owner).cloned().unwrap_or_default() };
    // The `.proto` lists retired numbers in `reserved`; it must agree.
    let declared = set
        .messages
        .iter()
        .map(|m| (&m.name, &m.reserved))
        .chain(set.enums.iter().map(|e| (&e.name, &e.reserved)));
    for (name, reserved) in declared {
        let mut reserved = reserved.clone();
        reserved.sort_unstable();
        reserved.dedup();
        let frozen = retired(name);
        if reserved != frozen {
            return Err(format!(
                "{name} reserves {reserved:?} in the .proto, but proto-freeze.txt retires {frozen:?}"
            ));
        }
    }

    // Sorted views and the id constants.
    let mut messages: Vec<&ProtoMessage> = set.messages.iter().collect();
    messages.sort_by(|a, b| a.name.cmp(&b.name));
    let mut enums: Vec<&ProtoEnum> = set.enums.iter().collect();
    enums.sort_by(|a, b| a.name.cmp(&b.name));
    let mut consts = BTreeSet::new();
    for name in messages
        .iter()
        .map(|m| &m.name)
        .chain(enums.iter().map(|e| &e.name))
    {
        if !consts.insert(screaming_snake(name)) {
            return Err(format!(
                "two declarations map to the constant {}",
                screaming_snake(name)
            ));
        }
    }

    let mut out = String::new();
    out.push_str(&header(
        "the .proto files and spec/1.0/proto-freeze.txt",
        commit,
    ));
    out.push_str(MODULE_DOC.trim_start());
    let _ = writeln!(
        out,
        "\n/// The protobuf package the field table describes.\npub const PACKAGE: &str = {};",
        string_literal(set.package.as_deref().unwrap_or(""))
    );
    let _ = writeln!(
        out,
        "/// The upstream commit the `.proto` files were read at (`spec.lock`).\n\
         pub const SPEC_COMMIT: &str = {};",
        string_literal(commit)
    );
    out.push_str(TYPES);

    let _ = writeln!(
        out,
        "/// Every message, nested ones included, sorted by dotted name. A [`MsgId`] indexes this.\n\
         pub static MESSAGES: [Message; {}] = [",
        messages.len()
    );
    for m in &messages {
        let mut fields: Vec<&ProtoField> = m.fields.iter().collect();
        fields.sort_by_key(|f| f.number);
        if fields.windows(2).any(|w| w[0].number == w[1].number) {
            return Err(format!("message {} reuses a field number", m.name));
        }
        let mut by_json: Vec<usize> = (0..fields.len()).collect();
        by_json.sort_by_key(|&i| json_name(&fields[i].name));
        if let Some(w) = by_json
            .windows(2)
            .find(|w| json_name(&fields[w[0]].name) == json_name(&fields[w[1]].name))
        {
            return Err(format!(
                "message {} has two fields with JSON name {}",
                m.name,
                json_name(&fields[w[0]].name)
            ));
        }
        if fields.len() > usize::from(u8::MAX) {
            return Err(format!("message {} has too many fields", m.name));
        }
        let _ = write!(
            out,
            "Message {{ name: {}, fields: &[",
            string_literal(&m.name)
        );
        for f in &fields {
            let kind = resolve(&f.type_name, &m.name, set)
                .map_err(|e| format!("{}.{}: {e}", m.name, f.name))?;
            let label = match f.label {
                Label::Implicit => "Implicit",
                Label::Optional => "Optional",
                Label::Repeated => "Repeated",
            };
            if f.oneof.is_some() && f.label != Label::Implicit {
                return Err(format!(
                    "{}.{}: a oneof member cannot carry a label",
                    m.name, f.name
                ));
            }
            let oneof = match &f.oneof {
                Some(g) => format!("Some({})", string_literal(g)),
                None => "None".to_owned(),
            };
            let _ = write!(
                out,
                "Field {{ name: {}, json_name: {}, number: {}, kind: Kind::{kind}, label: Label::{label}, oneof: {oneof} }},",
                string_literal(&f.name),
                string_literal(&json_name(&f.name)),
                f.number,
            );
        }
        let by_json: Vec<String> = by_json.iter().map(usize::to_string).collect();
        let reserved: Vec<String> = retired(&m.name).iter().map(i64::to_string).collect();
        let _ = writeln!(
            out,
            "], by_json_name: &[{}], reserved: &[{}] }},",
            by_json.join(", "),
            reserved.join(", ")
        );
    }
    out.push_str("];\n\n");

    let _ = writeln!(
        out,
        "/// Every enum, sorted by dotted name. An [`EnumId`] indexes this.\n\
         pub static ENUMS: [Enum; {}] = [",
        enums.len()
    );
    for e in &enums {
        let mut values: Vec<&(String, i32)> = e.values.iter().collect();
        values.sort_by_key(|v| v.1);
        if values.windows(2).any(|w| w[0].1 == w[1].1) {
            return Err(format!(
                "enum {} reuses a number (aliases are not supported)",
                e.name
            ));
        }
        let mut by_name: Vec<usize> = (0..values.len()).collect();
        by_name.sort_by_key(|&i| &values[i].0);
        if values.len() > usize::from(u8::MAX) {
            return Err(format!("enum {} has too many values", e.name));
        }
        let _ = write!(out, "Enum {{ name: {}, values: &[", string_literal(&e.name));
        for (name, number) in &values {
            let _ = write!(
                out,
                "EnumValue {{ name: {}, number: {number} }},",
                string_literal(name)
            );
        }
        let by_name: Vec<String> = by_name.iter().map(usize::to_string).collect();
        let reserved: Vec<String> = retired(&e.name).iter().map(i64::to_string).collect();
        let _ = writeln!(
            out,
            "], by_name: &[{}], reserved: &[{}] }},",
            by_name.join(", "),
            reserved.join(", ")
        );
    }
    out.push_str("];\n\n");

    for (i, m) in messages.iter().enumerate() {
        let _ = writeln!(
            out,
            "/// `message {}`.\npub const {}: MsgId = MsgId({i});",
            m.name,
            screaming_snake(&m.name)
        );
    }
    for (i, e) in enums.iter().enumerate() {
        let _ = writeln!(
            out,
            "/// `enum {}`.\npub const {}: EnumId = EnumId({i});",
            e.name,
            screaming_snake(&e.name)
        );
    }
    out.push_str(LOOKUPS);
    Ok(out)
}

const MODULE_DOC: &str = r#"
//! The protobuf field table: every message, field and enum of the AG-UI
//! `.proto` files as `static` data, for the hand-written codec.
//!
//! The `.proto` files give each field's type and label; `proto-freeze.txt`
//! is the authority on wire numbers, and the generator refuses a `.proto`
//! whose numbers disagree with it. A frozen slot of a declared message or
//! enum that no longer has a field or value is listed in `reserved`, so a
//! decoder can tell a retired slot from an unknown one.
//!
//! * [`MESSAGES`] and [`ENUMS`] are sorted by dotted name (`ToolCall.Function`);
//!   [`MsgId`] and [`EnumId`] index them, with one named constant each.
//! * A message's fields are sorted by number for [`Message::field`];
//!   `by_json_name` orders them by JSON name for [`Message::field_by_json_name`].
//!   Both are binary searches.
//! * `json_name` is proto3's default JSON name (lower camel case of the field
//!   name). Where the JSON shape differs from the message (content parts,
//!   outcomes), the codec maps it; the table only describes the wire.
"#;

const TYPES: &str = r#"
/// Identifies a message: an index into [`MESSAGES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MsgId(pub u16);

impl MsgId {
    /// The message this id names.
    pub fn message(self) -> &'static Message {
        &MESSAGES[usize::from(self.0)]
    }
}

/// Identifies an enum: an index into [`ENUMS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnumId(pub u16);

impl EnumId {
    /// The enum this id names.
    pub fn enumeration(self) -> &'static Enum {
        &ENUMS[usize::from(self.0)]
    }
}

/// A field's cardinality and presence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    /// Singular without `optional`: a scalar's default value is not
    /// written; a message-typed field still has presence. Oneof members
    /// carry this label.
    Implicit,
    /// `optional`: explicit presence.
    Optional,
    /// `repeated` (scalars packed, as proto3 does by default).
    Repeated,
}

/// A field's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `bool` (varint).
    Bool,
    /// `bytes` (length-delimited).
    Bytes,
    /// `double` (64-bit).
    Double,
    /// `float` (32-bit).
    Float,
    /// `int32` (varint).
    Int32,
    /// `int64` (varint).
    Int64,
    /// `string` (length-delimited UTF-8).
    String,
    /// `uint32` (varint).
    Uint32,
    /// `uint64` (varint).
    Uint64,
    /// `google.protobuf.Struct` (length-delimited).
    Struct,
    /// `google.protobuf.Value` (length-delimited).
    Value,
    /// An enum (varint).
    Enum(EnumId),
    /// A message (length-delimited).
    Message(MsgId),
}

impl Kind {
    /// The wire type a value of this kind is encoded with: 0 varint,
    /// 1 64-bit, 2 length-delimited, 5 32-bit.
    pub fn wire_type(self) -> u8 {
        match self {
            Kind::Bool
            | Kind::Int32
            | Kind::Int64
            | Kind::Uint32
            | Kind::Uint64
            | Kind::Enum(_) => 0,
            Kind::Double => 1,
            Kind::Float => 5,
            Kind::Bytes | Kind::String | Kind::Struct | Kind::Value | Kind::Message(_) => 2,
        }
    }
}

/// One message field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// The `.proto` field name (snake case).
    pub name: &'static str,
    /// The proto3 default JSON name (lower camel case).
    pub json_name: &'static str,
    /// The frozen field number.
    pub number: u32,
    /// The field's type.
    pub kind: Kind,
    /// Cardinality and presence.
    pub label: Label,
    /// The `oneof` group the field belongs to.
    pub oneof: Option<&'static str>,
}

/// One message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message {
    /// Dotted name relative to the package.
    pub name: &'static str,
    /// The fields, sorted by number.
    pub fields: &'static [Field],
    /// Indexes into `fields`, ordered by `json_name`.
    pub by_json_name: &'static [u8],
    /// Frozen numbers of retired fields: never reuse, skip on decode.
    pub reserved: &'static [u32],
}

impl Message {
    /// The field with this number (binary search).
    pub fn field(&self, number: u32) -> Option<&'static Field> {
        self.fields
            .binary_search_by_key(&number, |f| f.number)
            .ok()
            .and_then(|i| self.fields.get(i))
    }

    /// The field with this JSON name (binary search).
    pub fn field_by_json_name(&self, json_name: &str) -> Option<&'static Field> {
        let fields = self.fields;
        self.by_json_name
            .binary_search_by(|&i| {
                fields
                    .get(usize::from(i))
                    .map_or("", |f| f.json_name)
                    .cmp(json_name)
            })
            .ok()
            .and_then(|i| self.by_json_name.get(i))
            .and_then(|&i| fields.get(usize::from(i)))
    }
}

/// One enum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnumValue {
    /// The value's name.
    pub name: &'static str,
    /// Its frozen number.
    pub number: i32,
}

/// One enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enum {
    /// Dotted name relative to the package.
    pub name: &'static str,
    /// The values, sorted by number.
    pub values: &'static [EnumValue],
    /// Indexes into `values`, ordered by name.
    pub by_name: &'static [u8],
    /// Frozen numbers of retired values.
    pub reserved: &'static [i32],
}

impl Enum {
    /// The name of the value with this number (binary search).
    pub fn name_of(&self, number: i32) -> Option<&'static str> {
        self.values
            .binary_search_by_key(&number, |v| v.number)
            .ok()
            .and_then(|i| self.values.get(i))
            .map(|v| v.name)
    }

    /// The number of the value with this name (binary search).
    pub fn number_of(&self, name: &str) -> Option<i32> {
        let values = self.values;
        self.by_name
            .binary_search_by(|&i| values.get(usize::from(i)).map_or("", |v| v.name).cmp(name))
            .ok()
            .and_then(|i| self.by_name.get(i))
            .and_then(|&i| values.get(usize::from(i)))
            .map(|v| v.number)
    }
}

"#;

const LOOKUPS: &str = r#"
/// The message with this dotted name (binary search over [`MESSAGES`]).
pub fn message(name: &str) -> Option<MsgId> {
    MESSAGES
        .binary_search_by(|m| m.name.cmp(name))
        .ok()
        .and_then(|i| u16::try_from(i).ok())
        .map(MsgId)
}

/// The enum with this dotted name (binary search over [`ENUMS`]).
pub fn enumeration(name: &str) -> Option<EnumId> {
    ENUMS
        .binary_search_by(|e| e.name.cmp(name))
        .ok()
        .and_then(|i| u16::try_from(i).ok())
        .map(EnumId)
}
"#;

#[cfg(test)]
mod tests {
    use super::{parse, parse_freeze, render};

    fn gen(proto: &str, freeze: &str) -> Result<String, String> {
        let set = parse(&[("t.proto".to_owned(), proto.to_owned())])?;
        render(&set, &parse_freeze(freeze)?, "c0ffee")
    }

    /// The error `gen` must return.
    fn refused(proto: &str, freeze: &str) -> String {
        match gen(proto, freeze) {
            Err(err) => err,
            Ok(_) => "accepted".to_owned(),
        }
    }

    /// The `reserved` list rendered for the declaration named `name`.
    fn reserved_of(out: &str, name: &str) -> Result<String, String> {
        let start = out
            .find(&format!("name: \"{name}\", "))
            .ok_or(format!("no {name} in output"))?;
        let rest = &out[start..];
        let open = "reserved: &[";
        let r = rest.find(open).ok_or("no reserved list")? + open.len();
        let end = rest[r..].find(']').ok_or("unterminated reserved list")?;
        Ok(rest[r..r + end].to_owned())
    }

    #[test]
    fn reserved_statements_parse_and_match_the_freeze() -> Result<(), String> {
        let proto = "syntax = \"proto3\";\n\
                     message M { reserved 2, 4; string a = 1; }\n\
                     enum E { E_A = 0; reserved 1; }\n";
        let freeze = "M.a = 1\nM.b = 2\nM.c = 4\nE.E_A = 0\nE.E_B = 1\n";
        let out = gen(proto, freeze)?;
        assert_eq!(reserved_of(&out, "M")?, "2, 4");
        assert_eq!(reserved_of(&out, "E")?, "1");
        Ok(())
    }

    #[test]
    fn reserved_statement_disagreeing_with_the_freeze_is_refused() {
        let proto = "syntax = \"proto3\";\nmessage M { reserved 3; string a = 1; }\n";
        let err = refused(proto, "M.a = 1\nM.b = 2\n");
        assert!(err.contains("retires [2]"), "{err}");
    }

    #[test]
    fn reserved_names_and_ranges_are_refused() {
        for (body, why) in [
            ("reserved \"b\";", "reserved names"),
            ("reserved 2 to 4;", "reserved ranges"),
        ] {
            let proto = format!("syntax = \"proto3\";\nmessage M {{ {body} string a = 1; }}\n");
            let err = refused(&proto, "M.a = 1\n");
            assert!(err.contains(why), "{err}");
        }
    }

    #[test]
    fn slot_of_a_deleted_nested_message_is_not_filed_under_its_parent() -> Result<(), String> {
        let proto = "syntax = \"proto3\";\nmessage Outer { string a = 1; }\n";
        let out = gen(proto, "Outer.a = 1\nOuter.Inner.x = 1\n")?;
        assert_eq!(reserved_of(&out, "Outer")?, "");
        Ok(())
    }

    #[test]
    fn retired_oneof_member_is_filed_under_its_message() -> Result<(), String> {
        let proto = "syntax = \"proto3\";\nmessage M { reserved 2; string a = 1; }\n";
        let out = gen(proto, "M.a = 1\nM.kind.b = 2\n")?;
        assert_eq!(reserved_of(&out, "M")?, "2");
        Ok(())
    }

    #[test]
    fn retired_number_equal_to_a_live_number_is_refused() {
        let proto = "syntax = \"proto3\";\nmessage M { string a = 1; }\n";
        let err = refused(proto, "M.a = 1\nM.b = 1\n");
        assert!(err.contains("still uses 1"), "{err}");
    }

    #[test]
    fn two_fields_with_one_json_name_are_refused() {
        let proto = "syntax = \"proto3\";\nmessage M { string foo_bar = 1; string fooBar = 2; }\n";
        let err = refused(proto, "M.foo_bar = 1\nM.fooBar = 2\n");
        assert!(err.contains("JSON name"), "{err}");
    }

    #[test]
    fn missing_syntax_is_refused() {
        let err = refused("message M { string a = 1; }\n", "M.a = 1\n");
        assert!(err.contains("syntax"), "{err}");
    }
}
