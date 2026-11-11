//! Keeps the codec's static tables (`schema.rs`) in sync with the wire
//! contract: parses the pinned upstream `.proto` files and `proto-freeze.txt`
//! from the spec cache and compares every message, field, oneof arm and enum
//! value with the tables. Skips (visibly) when the cache is missing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::schema::{
    Body, Json, Label, Ty, ALL_ENUMS, ALL_MESSAGES, EVENT, EVENT_TYPE, MAX_FIELDS,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::test_spec as proto_support;

/// (type, name, number) of a oneof arm.
type PArm = (String, String, u32);

#[derive(Debug, Default)]
struct PMsg {
    /// (label, type, name, number)
    fields: Vec<(String, String, String, u32)>,
    /// oneof name -> (type, name, number)
    oneofs: Vec<(String, Vec<PArm>)>,
}

#[derive(Debug, Default)]
struct Proto {
    msgs: BTreeMap<String, PMsg>,
    enums: BTreeMap<String, Vec<(String, i32)>>,
}

fn tokens(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let line = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        let mut cur = String::new();
        let mut in_str = false;
        for c in line.chars() {
            if in_str {
                cur.push(c);
                if c == '"' {
                    in_str = false;
                    out.push(std::mem::take(&mut cur));
                }
                continue;
            }
            match c {
                '"' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    in_str = true;
                    cur.push(c);
                }
                '{' | '}' | ';' | '=' | '<' | '>' | ',' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    out.push(c.to_string());
                }
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

struct P {
    t: Vec<String>,
    i: usize,
}

impl P {
    fn next(&mut self) -> String {
        let s = self.t[self.i].clone();
        self.i += 1;
        s
    }
    fn peek(&self) -> &str {
        &self.t[self.i]
    }
    fn expect(&mut self, s: &str) {
        let got = self.next();
        assert_eq!(got, s, "proto parse: expected {s:?}");
    }
    fn skip_statement(&mut self) {
        while self.next() != ";" {}
    }
    fn enum_body(&mut self, name: String, out: &mut Proto) {
        self.expect("{");
        let mut vals = Vec::new();
        while self.peek() != "}" {
            let id = self.next();
            if id == "option" || id == "reserved" {
                self.skip_statement();
                continue;
            }
            self.expect("=");
            let n: i32 = self.next().parse().unwrap();
            self.expect(";");
            vals.push((id, n));
        }
        self.expect("}");
        out.enums.insert(name, vals);
    }
    fn field(&mut self) -> (String, String, String, u32) {
        let mut label = String::new();
        let mut ty = self.next();
        if ty == "optional" || ty == "repeated" {
            label = ty;
            ty = self.next();
        }
        let name = self.next();
        self.expect("=");
        let n: u32 = self.next().parse().unwrap();
        self.expect(";");
        (label, ty, name, n)
    }
    fn message(&mut self, name: String, out: &mut Proto) {
        self.expect("{");
        let mut m = PMsg::default();
        while self.peek() != "}" {
            match self.peek() {
                "message" => {
                    self.next();
                    let n = self.next();
                    self.message(format!("{name}.{n}"), out);
                }
                "enum" => {
                    self.next();
                    let n = self.next();
                    self.enum_body(format!("{name}.{n}"), out);
                }
                "oneof" => {
                    self.next();
                    let on = self.next();
                    self.expect("{");
                    let mut arms = Vec::new();
                    while self.peek() != "}" {
                        let (_, ty, n, num) = self.field();
                        arms.push((ty, n, num));
                    }
                    self.expect("}");
                    m.oneofs.push((on, arms));
                }
                "option" | "reserved" => self.skip_statement(),
                _ => m.fields.push(self.field()),
            }
        }
        self.expect("}");
        out.msgs.insert(name, m);
    }
}

fn parse_protos() -> Option<Proto> {
    let dir = proto_support::proto_sources_dir()?;
    let mut out = Proto::default();
    for f in ["events.proto", "types.proto", "patch.proto"] {
        let src = std::fs::read_to_string(dir.join(f)).unwrap();
        let mut p = P {
            t: tokens(&src),
            i: 0,
        };
        while p.i < p.t.len() {
            match p.next().as_str() {
                "message" => {
                    let n = p.next();
                    p.message(n, &mut out);
                }
                "enum" => {
                    let n = p.next();
                    p.enum_body(n, &mut out);
                }
                _ => p.skip_statement(),
            }
        }
    }
    Some(out)
}

fn last_segment(s: &str) -> &str {
    s.rsplit('.').next().unwrap()
}

fn ty_name(t: Ty) -> String {
    match t {
        Ty::String => "string".into(),
        Ty::Bool => "bool".into(),
        Ty::Int64 => "int64".into(),
        Ty::Enum(e) => e.name.into(),
        Ty::Message(m) => last_segment(m.name).into(),
        Ty::Value => "google.protobuf.Value".into(),
        Ty::Struct => "google.protobuf.Struct".into(),
    }
}

fn label_name(l: Label) -> &'static str {
    match l {
        Label::Implicit => "",
        Label::Optional => "optional",
        Label::Repeated => "repeated",
    }
}

fn lower_camel(s: &str) -> String {
    let mut out = String::new();
    let mut up = false;
    for c in s.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn tables_match_proto_files() {
    let Some(proto) = parse_protos() else {
        return;
    };
    let table: BTreeSet<&str> = ALL_MESSAGES.iter().map(|m| m.name).collect();
    let parsed: BTreeSet<&str> = proto.msgs.keys().map(|s| s.as_str()).collect();
    assert_eq!(table, parsed, "message sets differ");
    assert_eq!(table.len(), ALL_MESSAGES.len(), "duplicate in ALL_MESSAGES");

    for m in ALL_MESSAGES {
        let pm = &proto.msgs[m.name];
        match m.body {
            Body::Fields(fs) => {
                assert!(pm.oneofs.is_empty(), "{}: unexpected oneof", m.name);
                assert!(fs.len() <= MAX_FIELDS, "{}: too many fields", m.name);
                assert!(
                    fs.windows(2).all(|w| w[0].number < w[1].number),
                    "{}: unsorted",
                    m.name
                );
                let mine: BTreeSet<(String, String, String, u32)> = fs
                    .iter()
                    .map(|f| {
                        (
                            label_name(f.label).to_string(),
                            ty_name(f.ty),
                            f.proto.to_string(),
                            f.number,
                        )
                    })
                    .collect();
                let theirs: BTreeSet<(String, String, String, u32)> = pm
                    .fields
                    .iter()
                    .map(|(l, t, n, num)| (l.clone(), t.clone(), n.clone(), *num))
                    .collect();
                assert_eq!(mine, theirs, "{}: fields differ", m.name);
                for f in fs {
                    if let Json::Key(k) = f.json {
                        assert_eq!(k, lower_camel(f.proto), "{}.{}: JSON key", m.name, f.proto);
                    }
                }
            }
            Body::Union { oneof, arms } => {
                assert!(pm.fields.is_empty(), "{}: fields outside the oneof", m.name);
                assert_eq!(pm.oneofs.len(), 1, "{}", m.name);
                let (on, parms) = &pm.oneofs[0];
                assert_eq!(on, oneof, "{}: oneof name", m.name);
                let mine: BTreeSet<(String, String, u32)> = arms
                    .iter()
                    .map(|a| {
                        (
                            last_segment(a.msg.name).to_string(),
                            a.proto.to_string(),
                            a.number,
                        )
                    })
                    .collect();
                let theirs: BTreeSet<(String, String, u32)> = parms.iter().cloned().collect();
                assert_eq!(mine, theirs, "{}: arms differ", m.name);
                assert!(
                    arms.windows(2).all(|w| w[0].number < w[1].number),
                    "{}: unsorted",
                    m.name
                );
            }
        }
    }

    for e in ALL_ENUMS {
        let mine: Vec<(String, i32)> = e
            .values
            .iter()
            .map(|v| (v.proto.to_string(), v.number))
            .collect();
        assert_eq!(&mine, &proto.enums[e.name], "enum {}", e.name);
    }
    assert_eq!(ALL_ENUMS.len(), proto.enums.len());
}

#[test]
fn event_arms_match_event_type() {
    let Body::Union { arms, .. } = EVENT.body else {
        panic!()
    };
    assert_eq!(arms.len(), EVENT_TYPE.values.len());
    for a in arms {
        let v = EVENT_TYPE.by_json(a.json).expect(a.json);
        assert_eq!(v.number, a.event_type, "{}", a.json);
        assert_eq!(a.proto.to_uppercase(), a.json);
        let expected_msg: String = a
            .proto
            .split('_')
            .map(|w| {
                let mut c = w.chars();
                c.next()
                    .unwrap()
                    .to_uppercase()
                    .chain(c)
                    .collect::<String>()
            })
            .collect::<String>();
        assert_eq!(a.msg.name, format!("{expected_msg}Event"));
    }
}

#[test]
fn tables_match_freeze() {
    let Some(path) = proto_support::proto_freeze() else {
        return;
    };
    let src = std::fs::read_to_string(path).unwrap();
    let freeze: BTreeMap<String, i64> = src
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(|l| {
            let (k, v) = l.split_once('=').unwrap();
            (k.trim().to_string(), v.trim().parse().unwrap())
        })
        .collect();
    let mut seen = BTreeSet::new();
    let mut check = |k: String, n: i64| {
        assert_eq!(freeze.get(&k), Some(&n), "freeze slot {k}");
        seen.insert(k);
    };
    for m in ALL_MESSAGES {
        match m.body {
            Body::Fields(fs) => {
                for f in fs {
                    check(format!("{}.{}", m.name, f.proto), f.number as i64);
                }
            }
            Body::Union { oneof, arms } => {
                for a in arms {
                    check(format!("{}.{}.{}", m.name, oneof, a.proto), a.number as i64);
                }
            }
        }
    }
    for e in ALL_ENUMS {
        for v in e.values {
            check(format!("{}.{}", e.name, v.proto), v.number as i64);
        }
    }
    let missing: Vec<_> = freeze.keys().filter(|k| !seen.contains(*k)).collect();
    assert!(
        missing.is_empty(),
        "freeze slots not in the tables: {missing:?}"
    );
}
