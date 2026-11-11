//! The traceability gate and progress report, from the command line.
//!
//! ```text
//! agui-trace gate [--no-pending]       exit 1 on any problem
//! agui-trace progress [--out FILE]     write the progress report (default docs/compliance/progress.md)
//! agui-trace progress --check          exit 1 if the committed report is out of date
//! ```

use std::process::ExitCode;

use agui_test_support::traceability::{analyze, progress_markdown, Inputs, Mode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let inputs = Inputs::repo();
    match args.first().map(String::as_str) {
        Some("gate") => {
            let mode = if args.iter().any(|a| a == "--no-pending") {
                Mode::NoPending
            } else {
                Mode::Pending
            };
            let report = analyze(&inputs, mode);
            let covered = report.rows.iter().filter(|r| r.covered()).count();
            for note in &report.notes {
                eprintln!("note: {note}");
            }
            if report.problems.is_empty() {
                println!(
                    "traceability gate: ok ({covered}/{} gated rows covered)",
                    report.rows.len()
                );
                ExitCode::SUCCESS
            } else {
                for p in &report.problems {
                    eprintln!("error: {p}");
                }
                eprintln!("traceability gate: {} problem(s)", report.problems.len());
                ExitCode::FAILURE
            }
        }
        Some("progress") => {
            let report = analyze(&inputs, Mode::Pending);
            let md = progress_markdown(&report);
            let out = args
                .iter()
                .position(|a| a == "--out")
                .and_then(|i| args.get(i + 1))
                .map_or_else(
                    || inputs.root.join("docs/compliance/progress.md"),
                    Into::into,
                );
            if args.iter().any(|a| a == "--check") {
                let current = std::fs::read_to_string(&out).unwrap_or_default();
                if current == md {
                    return ExitCode::SUCCESS;
                }
                eprintln!("{} is out of date: run `cargo run -p agui-test-support --bin agui-trace -- progress`", out.display());
                return ExitCode::FAILURE;
            }
            match std::fs::write(&out, md) {
                Ok(()) => {
                    println!("wrote {}", out.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}: {e}", out.display());
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!("usage: agui-trace <gate [--no-pending] | progress [--out FILE] [--check]>");
            ExitCode::from(2)
        }
    }
}
