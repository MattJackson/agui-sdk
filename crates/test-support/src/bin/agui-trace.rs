//! The traceability gate and progress report, from the command line.
//!
//! ```text
//! agui-trace gate [--no-pending]       exit 1 on any problem
//! agui-trace progress [--out FILE]     write the progress report (default docs/compliance/progress.md)
//! agui-trace progress --check          exit 1 if the committed report is out of date
//! ```

use std::process::ExitCode;

use agui_test_support::traceability::{analyze, progress_markdown, Inputs, Mode};

const USAGE: &str = "usage: agui-trace <gate [--no-pending] | progress [--out FILE] [--check]>";

/// The flags after the subcommand, parsed once.
#[derive(Debug, Default, PartialEq, Eq)]
struct Flags {
    no_pending: bool,
    check: bool,
    out: Option<String>,
}

/// Parses the arguments after the subcommand, or `None` when one is not a
/// flag the command knows (`--out` takes a value), so a misspelt flag is
/// refused rather than ignored. The value after `--out` is never also read
/// as a flag.
fn parse_flags(command: &str, rest: &[String]) -> Option<Flags> {
    let mut flags = Flags::default();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match (command, a.as_str()) {
            ("gate", "--no-pending") => flags.no_pending = true,
            ("progress", "--check") => flags.check = true,
            ("progress", "--out") => flags.out = Some(it.next()?.clone()),
            _ => return None,
        }
    }
    Some(flags)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let Some(flags) = parse_flags(command, &args[1..]) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let inputs = Inputs::repo();
    match command.as_str() {
        "gate" => {
            let mode = if flags.no_pending {
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
        "progress" => {
            let report = analyze(&inputs, Mode::Pending);
            let md = progress_markdown(&report);
            let out = flags.out.as_ref().map_or_else(
                || inputs.root.join("docs/compliance/progress.md"),
                Into::into,
            );
            if flags.check {
                // A missing report is out of date; any other read error is
                // reported as itself.
                let current = match std::fs::read_to_string(&out) {
                    Ok(t) => t,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(e) => {
                        eprintln!("{}: {e}", out.display());
                        return ExitCode::FAILURE;
                    }
                };
                if current == md {
                    return ExitCode::SUCCESS;
                }
                eprintln!(
                    "{} is out of date: run `{}`",
                    out.display(),
                    regenerate_command(flags.out.as_deref())
                );
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
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// The command that rewrites the report `--check` compared against.
fn regenerate_command(out: Option<&str>) -> String {
    let base = "cargo run -p agui-test-support --bin agui-trace -- progress";
    match out {
        Some(path) => format!("{base} --out {path}"),
        None => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_flags, regenerate_command, Flags};

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parse_flags_accepts_each_command_s_own_flags() {
        assert_eq!(parse_flags("gate", &args(&[])), Some(Flags::default()));
        assert!(parse_flags("gate", &args(&["--no-pending"])).is_some_and(|f| f.no_pending));
        assert!(parse_flags("progress", &args(&["--check"])).is_some_and(|f| f.check));
        assert_eq!(
            parse_flags("progress", &args(&["--out", "F", "--check"])),
            Some(Flags {
                no_pending: false,
                check: true,
                out: Some("F".to_owned()),
            })
        );
    }

    #[test]
    fn parse_flags_refuses_unknown_misspelt_or_incomplete_flags() {
        assert_eq!(parse_flags("gate", &args(&["--no-pendng"])), None);
        assert_eq!(parse_flags("gate", &args(&["--check"])), None);
        assert_eq!(parse_flags("progress", &args(&["--no-pending"])), None);
        assert_eq!(parse_flags("progress", &args(&["--out"])), None);
    }

    #[test]
    fn the_value_after_out_is_not_also_a_flag() {
        assert_eq!(
            parse_flags("progress", &args(&["--out", "--check"])),
            Some(Flags {
                no_pending: false,
                check: false,
                out: Some("--check".to_owned()),
            })
        );
    }

    #[test]
    fn the_check_hint_names_the_checked_file() {
        assert!(regenerate_command(None).ends_with("-- progress"));
        assert!(regenerate_command(Some("x/p.md")).ends_with("-- progress --out x/p.md"));
    }
}
