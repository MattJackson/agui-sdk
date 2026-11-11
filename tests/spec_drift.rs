//! Drift: the generated tables must equal what the pinned spec produces.
//!
//! The generator (`xtask/src/codegen`) and the spec-cache helper
//! (`crates/test-support/src/spec.rs`) are standard-library-only and are
//! compiled straight into this test, as are the two generated modules, so
//! this file also proves the committed tables build and answer lookups.
//!
//! Skips with a message when the spec cache is missing (fails under
//! `AGUI_REQUIRE_SPEC=1`); run `cargo xtask spec-fetch` first. Regenerating
//! pipes through `rustfmt`, which must be installed.
//!
//! Still to come with the protocol types: comparing the hand-written
//! structs' serde field sets and enum value sets with `schema.json`
//! (TEST-PLAN §6, second half).

#[allow(dead_code)]
#[path = "../xtask/src/codegen/mod.rs"]
mod codegen;

use agui_test_support::spec;

#[allow(dead_code)]
#[path = "../src/pipeline/shapes.rs"]
mod shapes;

#[allow(dead_code)]
#[path = "../src/proto/fields.rs"]
mod fields;

use std::path::Path;

#[test]
fn generated_tables_match_the_pinned_spec() {
    let Some(cache) = spec::root() else {
        return;
    };
    let commit = spec::pinned_commit();
    let outputs = match codegen::generate(&cache, &commit) {
        Ok(outputs) => outputs,
        Err(e) => panic!("regenerating from the pinned spec failed: {e}"),
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let stale: Vec<&str> = outputs
        .iter()
        .filter(|out| {
            std::fs::read_to_string(root.join(out.path)).ok().as_deref()
                != Some(out.contents.as_str())
        })
        .map(|out| out.path)
        .collect();
    assert!(
        stale.is_empty(),
        "{stale:?} differ from what spec {commit} produces: run 'cargo xtask codegen'"
    );
}

#[test]
fn committed_tables_name_the_pinned_commit() {
    let commit = spec::pinned_commit();
    assert_eq!(shapes::SPEC_COMMIT, commit);
    assert_eq!(fields::SPEC_COMMIT, commit);
}

#[test]
fn shape_table_lookups() {
    use shapes::{DefKind, Openness, Ty};

    for (i, def) in shapes::DEFS.iter().enumerate() {
        assert_eq!(
            shapes::lookup(def.name).map(|id| usize::from(id.0)),
            Some(i)
        );
    }
    assert_eq!(shapes::lookup("NoSuchDefinition"), None);

    let started = shapes::event("RUN_STARTED");
    assert_eq!(started, Some(shapes::RUN_STARTED_EVENT));
    assert_eq!(shapes::event("THINKING_START"), None);

    let DefKind::Object(obj) = shapes::TEXT_MESSAGE_START_EVENT.def().kind else {
        panic!("TextMessageStartEvent is not an object");
    };
    assert_eq!(obj.openness, Openness::Closed);
    assert_eq!(obj.fragments, [shapes::BASE_EVENT, shapes::ATTRIBUTABLE]);
    let ty = obj.field("type").map(|f| (f.ty, f.required));
    assert_eq!(ty, Some((Ty::Const("TEXT_MESSAGE_START"), true)));
    let role = obj.field("role");
    assert_eq!(role.and_then(|f| f.default), Some("\"assistant\""));
    assert!(obj
        .field("rawEvent")
        .is_some_and(|f| f.compat_null && !f.nullable));
    assert!(obj.field("unknown").is_none());

    let DefKind::Object(custom) = shapes::CUSTOM_EVENT.def().kind else {
        panic!("CustomEvent is not an object");
    };
    assert!(custom
        .field("value")
        .is_some_and(|f| f.required && f.nullable && !f.compat_null));

    let DefKind::Union(message) = shapes::MESSAGE.def().kind else {
        panic!("Message is not a union");
    };
    assert_eq!(message.discriminator, "role");
    assert_eq!(message.arm("tool"), Some(shapes::TOOL_MESSAGE));
    assert_eq!(message.arm("robot"), None);

    let DefKind::Object(add) = shapes::ADD_OPERATION.def().kind else {
        panic!("AddOperation is not an object");
    };
    assert_eq!(add.openness, Openness::Open);

    let DefKind::Enum(roles) = shapes::ROLE.def().kind else {
        panic!("Role is not an enum");
    };
    assert!(shapes::contains(roles, "reasoning"));
    assert!(!shapes::contains(roles, "robot"));

    // Every compat-null field is optional and rejects null, and vice versa.
    for def in &shapes::DEFS {
        if let DefKind::Object(obj) = def.kind {
            for f in obj.fields {
                assert_eq!(
                    f.compat_null,
                    !f.required && !f.nullable,
                    "{}.{}",
                    def.name,
                    f.name
                );
                if obj.openness != Openness::Fragment {
                    let listed = shapes::COMPAT_NULL_FIELDS.contains(&(def.name, f.name));
                    assert_eq!(listed, f.compat_null, "{}.{}", def.name, f.name);
                }
            }
        }
    }
}

#[test]
fn field_table_lookups() {
    use fields::{Kind, Label};

    for (i, m) in fields::MESSAGES.iter().enumerate() {
        assert_eq!(fields::message(m.name).map(|id| usize::from(id.0)), Some(i));
        for f in m.fields {
            assert_eq!(m.field(f.number), Some(f));
            assert_eq!(m.field_by_json_name(f.json_name), Some(f));
        }
    }
    let event = fields::EVENT.message();
    let start = event.field(12);
    assert_eq!(start.map(|f| f.name), Some("run_started"));
    assert_eq!(start.and_then(|f| f.oneof), Some("event"));
    assert_eq!(
        start.map(|f| f.kind),
        Some(Kind::Message(fields::RUN_STARTED_EVENT))
    );

    let base = fields::BASE_EVENT.message();
    let raw = base.field_by_json_name("rawEvent");
    assert_eq!(
        raw.map(|f| (f.number, f.kind, f.label)),
        Some((3, Kind::Value, Label::Optional))
    );
    assert_eq!(raw.map(|f| f.kind.wire_type()), Some(2));

    let types = fields::EVENT_TYPE.enumeration();
    assert_eq!(types.name_of(0), Some("TEXT_MESSAGE_START"));
    assert_eq!(types.number_of("REASONING_ENCRYPTED_VALUE"), Some(30));
    assert_eq!(types.number_of("THINKING_START"), None);
    assert_eq!(
        fields::enumeration("JsonPatchOperationType"),
        Some(fields::JSON_PATCH_OPERATION_TYPE)
    );
}
