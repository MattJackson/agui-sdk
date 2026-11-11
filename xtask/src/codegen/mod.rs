//! Code generation: the enforcement shape table from `schema.json` and the
//! protobuf field table from the `.proto` files and `proto-freeze.txt`.
//!
//! Standard library only, because `tests/spec_drift.rs` compiles this module
//! directly (`#[path]`) to regenerate the tables and compare them with the
//! committed files. The output is deterministic: every table is sorted
//! explicitly and nothing depends on map iteration order. It is then piped
//! through `rustfmt`, so the committed files are exactly what `cargo fmt`
//! would leave.

pub mod json;
pub mod proto;
pub mod schema;
pub mod shapes;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// The generated enforcement shape table, relative to the workspace root.
pub const SHAPES_PATH: &str = "src/pipeline/shapes.rs";
/// The generated protobuf field table, relative to the workspace root.
pub const FIELDS_PATH: &str = "src/proto/fields.rs";

/// Inputs, relative to the spec cache root.
pub const SCHEMA_FILE: &str = "spec/1.0/schema.json";
/// The protobuf wire-number freeze.
pub const FREEZE_FILE: &str = "spec/1.0/proto-freeze.txt";
/// The directory holding the `.proto` files.
pub const PROTO_DIR: &str = "sdks/typescript/packages/proto/src/proto";

/// One generated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Path relative to the workspace root.
    pub path: &'static str,
    /// The formatted contents.
    pub contents: String,
}

/// Generates both tables from the spec cache at `spec_root`, read at
/// upstream `commit`, formatted with `rustfmt`.
pub fn generate(spec_root: &Path, commit: &str) -> Result<Vec<Output>, String> {
    let read = |rel: &str| -> Result<String, String> {
        std::fs::read_to_string(spec_root.join(rel))
            .map_err(|e| format!("cannot read {}: {e}", spec_root.join(rel).display()))
    };

    let schema_doc = json::parse(&read(SCHEMA_FILE)?).map_err(|e| format!("{SCHEMA_FILE}: {e}"))?;
    let schema = schema::compile(&schema_doc).map_err(|e| format!("{SCHEMA_FILE}: {e}"))?;
    let shapes_src = shapes::render(&schema, commit)?;

    let proto_dir = spec_root.join(PROTO_DIR);
    let mut proto_files = Vec::new();
    let entries = std::fs::read_dir(&proto_dir)
        .map_err(|e| format!("cannot list {}: {e}", proto_dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("proto") {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            let src = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            proto_files.push((name, src));
        }
    }
    if proto_files.is_empty() {
        return Err(format!("no .proto files in {}", proto_dir.display()));
    }
    proto_files.sort();
    let set = proto::parse(&proto_files)?;
    let freeze = proto::parse_freeze(&read(FREEZE_FILE)?)?;
    let fields_src = proto::render(&set, &freeze, commit)?;

    Ok(vec![
        Output {
            path: SHAPES_PATH,
            contents: rustfmt(&shapes_src)?,
        },
        Output {
            path: FIELDS_PATH,
            contents: rustfmt(&fields_src)?,
        },
    ])
}

/// Formats Rust source with `rustfmt` (`$RUSTFMT` if set), edition 2021,
/// default style (the repository's `rustfmt.toml` sets only the edition).
pub fn rustfmt(src: &str) -> Result<String, String> {
    let program = std::env::var("RUSTFMT").unwrap_or_else(|_| "rustfmt".to_owned());
    run_rustfmt(&program, src)
}

fn run_rustfmt(program: &str, src: &str) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(["--edition", "2021", "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!("cannot run {program}: {e} (install it with `rustup component add rustfmt`)")
        })?;
    let mut stdin = child.stdin.take().ok_or("rustfmt has no stdin")?;
    let input = src.to_owned();
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let written = writer
        .join()
        .map_err(|_| "rustfmt writer panicked".to_owned())?;
    if !output.status.success() {
        let cause = match written {
            Err(e) => format!(" (writing its input: {e})"),
            Ok(()) => String::new(),
        };
        return Err(format!(
            "rustfmt failed on generated code: {}{cause}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    written.map_err(|e| e.to_string())?;
    String::from_utf8(output.stdout).map_err(|_| "rustfmt produced non-UTF-8 output".to_owned())
}

/// The banner every generated file starts with.
fn header(source: &str, commit: &str) -> String {
    format!(
        "// @generated by `cargo xtask codegen` from {source}\n\
         // at upstream commit {commit}.\n\
         // Do not edit by hand: regenerate. `tests/spec_drift.rs` fails when this\n\
         // file differs from what the pinned spec produces.\n\n"
    )
}

/// `ToolCall.Function` → `TOOL_CALL_FUNCTION`.
fn screaming_snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        if c == '.' || c == '_' {
            out.push('_');
            continue;
        }
        if c.is_ascii_uppercase() && i > 0 {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(char::is_ascii_lowercase);
            if prev.is_ascii_lowercase()
                || prev.is_ascii_digit()
                || (prev.is_ascii_uppercase() && next_lower)
            {
                out.push('_');
            }
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

/// A Rust string literal for `s`.
fn string_literal(s: &str) -> String {
    format!("{s:?}")
}

#[cfg(test)]
mod tests {
    use super::screaming_snake;

    #[cfg(unix)]
    #[test]
    fn rustfmt_failure_reports_stderr_not_broken_pipe() {
        // `ls` rejects the arguments and never reads stdin, so the writer
        // gets EPIPE on a large input.
        match super::run_rustfmt("ls", &"x".repeat(1 << 20)) {
            Err(err) => assert!(err.contains("rustfmt failed on generated code"), "{err}"),
            Ok(_) => panic!("ls is not rustfmt"),
        }
    }

    #[test]
    fn constant_names() {
        assert_eq!(screaming_snake("ToolCall.Function"), "TOOL_CALL_FUNCTION");
        assert_eq!(
            screaming_snake("HumanInTheLoopCapabilities"),
            "HUMAN_IN_THE_LOOP_CAPABILITIES"
        );
        assert_eq!(
            screaming_snake("JsonPatchOperationType"),
            "JSON_PATCH_OPERATION_TYPE"
        );
        assert_eq!(screaming_snake("URLSource"), "URL_SOURCE");
    }
}
