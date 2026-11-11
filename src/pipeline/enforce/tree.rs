//! Enforcement over an already-parsed `serde_json::Value`, in place.
//!
//! Used after middleware (the frame is a `Value` by then), for outgoing run
//! input, for the value corpus, and as the fallback of the byte path when an
//! unrecognised value in a required position means a container must go (the
//! byte path cannot take back what it already handed to the visitor).
//!
//! Removal never copies: stripped members are taken out of their map,
//! dropped elements are compacted out of their vector, and everything that
//! survives stays where it is.

use serde_json::{Map, Value};

use crate::diagnostics::DiagnosticCode;

use super::report::{
    check_int, is_pointer, Fault, Findings, IntVal, Kind, Num, OneOf, Options, Path, Quoted,
    Removal,
};
use super::shape::{List, Obj, Shape, Union};

/// What became of one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Kept (possibly with members stripped inside it).
    Keep,
    /// Unusable: its holder must remove it.
    Drop(Removal),
}

/// The walker; one per document.
pub(crate) struct Tree<'o> {
    opts: &'o Options,
    findings: &'o mut Findings,
    entries: usize,
}

impl<'o> Tree<'o> {
    pub(crate) fn new(opts: &'o Options, findings: &'o mut Findings) -> Self {
        Tree {
            opts,
            findings,
            entries: 0,
        }
    }

    /// Walks the document root. A `Drop` verdict is not reported here: the
    /// caller decides what dropping the whole document means.
    pub(crate) fn root(&mut self, shape: Shape, v: &mut Value) -> Result<Verdict, Fault> {
        self.walk(shape, v, &Path::ROOT, 0)
    }

    fn enter(&self, level: usize, path: &Path<'_>) -> Result<(), Fault> {
        if level > self.opts.max_depth {
            return Err(Fault::depth(path, self.opts.max_depth));
        }
        Ok(())
    }

    fn add_entries(&mut self, n: usize, path: &Path<'_>) -> Result<(), Fault> {
        self.entries = self.entries.saturating_add(n);
        if self.entries > self.opts.max_entries {
            return Err(Fault::entries(path, self.opts.max_entries));
        }
        Ok(())
    }

    /// Charges an open (or removed) value to the budgets.
    fn count(&mut self, v: &Value, path: &Path<'_>, depth: usize) -> Result<(), Fault> {
        match v {
            Value::Array(items) => {
                self.enter(depth + 1, path)?;
                self.add_entries(items.len(), path)?;
                for item in items {
                    self.count(item, path, depth + 1)?;
                }
            }
            Value::Object(map) => {
                self.enter(depth + 1, path)?;
                self.add_entries(map.len(), path)?;
                for item in map.values() {
                    self.count(item, path, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn walk(
        &mut self,
        shape: Shape,
        v: &mut Value,
        path: &Path<'_>,
        depth: usize,
    ) -> Result<Verdict, Fault> {
        match shape {
            Shape::Any => self.count(v, path, depth)?,
            Shape::NotNull => {
                if v.is_null() {
                    return Err(Fault::mismatch(path, &"a non-null value", Kind::Null));
                }
                self.count(v, path, depth)?;
            }
            Shape::OpenObject => {
                if !v.is_object() {
                    return Err(Fault::mismatch(path, &"object", Kind::of(v)));
                }
                self.count(v, path, depth)?;
            }
            Shape::Str => {
                if !v.is_string() {
                    return Err(Fault::mismatch(path, &"string", Kind::of(v)));
                }
            }
            Shape::Pointer => match v {
                Value::String(s) if is_pointer(s) => {}
                Value::String(_) => {
                    return Err(Fault::invalid(
                        path,
                        format_args!("expected a JSON pointer (RFC 6901)"),
                    ))
                }
                other => return Err(Fault::mismatch(path, &"string", Kind::of(other))),
            },
            Shape::Bool => {
                if !v.is_boolean() {
                    return Err(Fault::mismatch(path, &"boolean", Kind::of(v)));
                }
            }
            Shape::Int { min, max } => {
                let Value::Number(n) = v else {
                    return Err(Fault::mismatch(path, &"integer", Kind::of(v)));
                };
                match check_int(Num::of(n), min, max) {
                    Ok(IntVal::U(u)) => {
                        if n.as_u64() != Some(u) {
                            *v = Value::from(u);
                        }
                    }
                    Ok(IntVal::I(i)) => {
                        if n.as_i64() != Some(i) {
                            *v = Value::from(i);
                        }
                    }
                    Err(why) => {
                        return Err(Fault::invalid(
                            path,
                            format_args!("{why} (allowed {min}..={max})"),
                        ))
                    }
                }
            }
            Shape::Const(c) => match v {
                Value::String(s) if s == c => {}
                Value::String(_) => {
                    return Err(Fault::invalid(path, format_args!("expected {}", Quoted(c))))
                }
                other => return Err(Fault::mismatch(path, &Quoted(c), Kind::of(other))),
            },
            Shape::Pinned { value, set } => match v {
                Value::String(s) if s.as_str() == value => {}
                Value::String(s) if set.contains(&s.as_str()) || self.opts.strict => {
                    return Err(Fault::invalid(
                        path,
                        format_args!("expected {}", Quoted(value)),
                    ))
                }
                Value::String(_) => {
                    return Ok(Verdict::Drop(Removal::new(
                        DiagnosticCode::UnknownEnumValue,
                    )))
                }
                other => return Err(Fault::mismatch(path, &Quoted(value), Kind::of(other))),
            },
            Shape::Restricted { set, domain } => match v {
                Value::String(s) if set.contains(&s.as_str()) => {}
                Value::String(s) if domain.contains(&s.as_str()) || self.opts.strict => {
                    return Err(Fault::invalid(
                        path,
                        format_args!("expected {}", OneOf(set)),
                    ))
                }
                Value::String(_) => {
                    return Ok(Verdict::Drop(Removal::new(
                        DiagnosticCode::UnknownEnumValue,
                    )))
                }
                other => return Err(Fault::mismatch(path, &"string", Kind::of(other))),
            },
            Shape::Enum(set) => match v {
                Value::String(s) if set.contains(&s.as_str()) => {}
                Value::String(_) => {
                    if self.opts.strict {
                        return Err(Fault::invalid(
                            path,
                            format_args!("unrecognised value of a closed set"),
                        ));
                    }
                    return Ok(Verdict::Drop(Removal::new(
                        DiagnosticCode::UnknownEnumValue,
                    )));
                }
                other => return Err(Fault::mismatch(path, &"string", Kind::of(other))),
            },
            Shape::Object(obj) => {
                let Value::Object(map) = v else {
                    return Err(Fault::mismatch(path, &"object", Kind::of(v)));
                };
                return self.object(obj, map, path, depth);
            }
            Shape::List(list) => {
                let Value::Array(items) = v else {
                    return Err(Fault::mismatch(path, &"array", Kind::of(v)));
                };
                return self.list(list, items, path, depth);
            }
            Shape::TextOrList(list) => match v {
                Value::String(_) => {}
                Value::Array(items) => return self.list(list, items, path, depth),
                other => return Err(Fault::mismatch(path, &"string or array", Kind::of(other))),
            },
            Shape::Union(u) => return self.union(u, v, path, depth),
        }
        Ok(Verdict::Keep)
    }

    fn union(
        &mut self,
        u: Union,
        v: &mut Value,
        path: &Path<'_>,
        depth: usize,
    ) -> Result<Verdict, Fault> {
        let Value::Object(map) = v else {
            return Err(Fault::mismatch(path, &"object", Kind::of(v)));
        };
        let variant = match map.get(u.tag) {
            None => {
                return Err(Fault::invalid(
                    path,
                    format_args!("missing discriminator `{}`", u.tag),
                ))
            }
            Some(Value::String(s)) => u.variant(s),
            Some(other) => {
                return Err(Fault::mismatch(
                    &path.key(u.tag),
                    &"string",
                    Kind::of(other),
                ))
            }
        };
        match variant {
            Some((_, obj)) => self.object(obj, map, path, depth),
            None if self.opts.strict => Err(Fault::invalid(
                &path.key(u.tag),
                format_args!("unrecognised `{}` of {}", u.tag, u.anchor),
            )),
            None => {
                self.count(v, path, depth)?;
                Ok(Verdict::Drop(Removal::new(u.unknown.code())))
            }
        }
    }

    fn object(
        &mut self,
        obj: Obj,
        map: &mut Map<String, Value>,
        path: &Path<'_>,
        depth: usize,
    ) -> Result<Verdict, Fault> {
        self.enter(depth + 1, path)?;
        self.add_entries(map.len(), path)?;

        // Undescribed members, and the `null`-means-absent shim.
        let mut failed: Option<Fault> = None;
        let opts = self.opts;
        map.retain(|key, value| {
            if failed.is_some() {
                return true;
            }
            let here = path.key(key);
            match obj.find(key) {
                None if obj.closed => {
                    if opts.strict {
                        failed = Some(Fault::invalid(
                            &here,
                            format_args!("unrecognised property of {}", obj.anchor),
                        ));
                        return true;
                    }
                    if let Err(e) = self.count(value, &here, depth + 1) {
                        failed = Some(e);
                        return true;
                    }
                    self.findings.push(DiagnosticCode::UnknownField, &here, || {
                        format!("unrecognised property of {} removed", obj.anchor)
                    });
                    false
                }
                None => {
                    // Open object (a JSON Patch operation): kept as is.
                    if let Err(e) = self.count(value, &here, depth + 1) {
                        failed = Some(e);
                    }
                    true
                }
                Some((_, field)) => {
                    if value.is_null()
                        && !field.required
                        && !field.shape.accepts_null()
                        && opts.null_as_absent
                        && !opts.strict
                        && obj.null_reads_absent(field.name)
                    {
                        self.findings.push(DiagnosticCode::OptionalNull, &here, || {
                            "`null` in an optional position read as absent".to_owned()
                        });
                        return false;
                    }
                    true
                }
            }
        });
        if let Some(e) = failed {
            return Err(e);
        }

        // Described members, in schema order. A required member that turns
        // out unusable takes this object with it, but the walk goes on so a
        // malformed sibling is still fatal whatever the member order.
        let mut dropped: Option<Removal> = None;
        for field in obj.fields() {
            let here = path.key(field.name);
            let verdict = match map.get_mut(field.name) {
                None if field.required => {
                    return Err(Fault::invalid(
                        path,
                        format_args!("missing required field `{}`", field.name),
                    ))
                }
                None => continue,
                Some(value) => self.walk(field.shape, value, &here, depth + 1)?,
            };
            if let Verdict::Drop(removal) = verdict {
                if field.required {
                    dropped = dropped.or(Some(removal.through(field.name)));
                } else {
                    map.retain(|k, _| k != field.name);
                    self.findings
                        .push(removal.code, &here, || removal.message());
                }
            }
        }
        Ok(dropped.map_or(Verdict::Keep, Verdict::Drop))
    }

    fn list(
        &mut self,
        list: List,
        items: &mut Vec<Value>,
        path: &Path<'_>,
        depth: usize,
    ) -> Result<Verdict, Fault> {
        self.enter(depth + 1, path)?;
        let len = items.len();
        self.add_entries(len, path)?;
        if len < list.min_items {
            return Err(Fault::invalid(
                path,
                format_args!("expected at least {} element(s)", list.min_items),
            ));
        }
        let mut kept = 0;
        let mut last: Option<Removal> = None;
        for i in 0..len {
            let here = path.index(i);
            match self.walk(list.items(), &mut items[i], &here, depth + 1)? {
                Verdict::Keep => {
                    items.swap(kept, i);
                    kept += 1;
                }
                Verdict::Drop(removal) => {
                    self.findings
                        .push(removal.code, &here, || removal.message());
                    last = Some(removal);
                }
            }
        }
        items.truncate(kept);
        match last {
            Some(removal) if kept < list.min_items => Ok(Verdict::Drop(removal)),
            _ => Ok(Verdict::Keep),
        }
    }
}
