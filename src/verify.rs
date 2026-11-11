//! Compiler output parsing: turning a toolchain's stderr into counts and a
//! terse summary.
//!
//! This is **presentation only**, never a verdict. The verdict for a verify run
//! is the process exit code — parsed error counts can be wrong (an unfamiliar
//! toolchain format, a wrapped build, a message that spells "error" in prose)
//! and must never flip a pass into a fail or the other way. See
//! `notes/verify-gate-design.md` §4.3 for why the exit code is the only judge.
//!
//! What parsing *is* for: the model should not have to read 4,000 characters of
//! raw rustc output to learn "3 errors in 2 files". That is the same job
//! [`crate::diagnostics`] does for the LSP cache, on the same shape of output.
//!
//! Pure functions over `&str` — no I/O, no async (see `crate` docs).

/// Truncation cap for a parsed summary. Matches the diagnostics tool's budget
/// and the old `verify` tool's `MAX_SUMMARY_CHARS`, so one model-face budget
/// covers both paths.
pub const MAX_SUMMARY_CHARS: usize = 4000;

/// The counts and summary extracted from a compiler's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedErrors {
    pub error_count: usize,
    pub warning_count: usize,
    /// `file:line:col  message` lines, truncated to [`MAX_SUMMARY_CHARS`].
    /// Empty when the parser recognised no diagnostics — which is not the same
    /// as "no errors"; the exit code decides that.
    pub summary: String,
}

impl ParsedErrors {
    fn clean() -> Self {
        Self {
            error_count: 0,
            warning_count: 0,
            summary: String::new(),
        }
    }
}

/// Parse a toolchain's combined stdout+stderr for the given verification
/// command.
///
/// `command` is the same string the runner spawned, used only to pick a
/// parser. Unknown toolchains fall back to [`parse_generic`], which counts
/// lines rather than pretending to understand a format it has not seen.
pub fn parse_errors(command: &str, output: &str) -> ParsedErrors {
    parser_for_command(command)(output)
}

/// The parser for a verification command, by toolchain family.
fn parser_for_command(command: &str) -> fn(&str) -> ParsedErrors {
    let lower = command.to_ascii_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    if lower.contains("tsc") {
        parse_tsc
    } else if first == "cargo" || lower.contains("rustc") {
        parse_rustc
    } else if lower.contains("javac") || lower.contains("mvn") || lower.contains("gradle") {
        parse_javac
    } else if matches!(first, "gcc" | "clang" | "clang++" | "g++" | "cc") {
        parse_gcc
    } else {
        // `make`, `cmake`, `npm run`, anything unrecognised: count lines, do
        // not claim to understand the format.
        parse_generic
    }
}

/// `error[E0308]: mismatched types` / `warning: unused variable: x`, each with
/// a following `--> file:line:col` locator line.
fn parse_rustc(output: &str) -> ParsedErrors {
    let mut out = ParsedErrors::clean();
    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let kind = if trimmed.starts_with("error") {
            1
        } else if trimmed.starts_with("warning") {
            2
        } else {
            continue;
        };
        // `error: aborting due to N previous errors` is a tally, not a finding.
        if trimmed.starts_with("error: aborting") || trimmed.starts_with("warning: generated") {
            continue;
        }
        if kind == 1 {
            out.error_count += 1;
        } else {
            out.warning_count += 1;
        }
        // The next `--> file:line:col` line is this diagnostic's locator.
        let mut locator = String::new();
        while let Some(peeked) = lines.peek() {
            let p = peeked.trim_start();
            if let Some(rest) = p.strip_prefix("--> ") {
                locator = rest.trim().to_string();
                let _ = lines.next();
                break;
            }
            if p.starts_with("error") || p.starts_with("warning") {
                break;
            }
            let _ = lines.next();
        }
        push_finding(&mut out, &locator, trimmed);
    }
    finish(out)
}

/// `src/a.ts(42,5): error TS2322: Type 'string' is not assignable...`
fn parse_tsc(output: &str) -> ParsedErrors {
    let mut out = ParsedErrors::clean();
    for line in output.lines() {
        let trimmed = line.trim();
        let Some((loc, rest)) = trimmed.split_once("): ") else {
            continue;
        };
        // `src/a.ts(42,5` → `src/a.ts:42:5`
        let Some((file, pos)) = loc.rsplit_once('(') else {
            continue;
        };
        let locator = format!("{}:{}", file, pos.replace(',', ":"));
        if rest.starts_with("error") {
            out.error_count += 1;
        } else if rest.starts_with("warning") {
            out.warning_count += 1;
        } else {
            continue;
        }
        push_finding(&mut out, &locator, rest);
    }
    finish(out)
}

/// `a.java:42: error: incompatible types`
fn parse_javac(output: &str) -> ParsedErrors {
    let mut out = ParsedErrors::clean();
    for line in output.lines() {
        let trimmed = line.trim();
        let Some((loc, rest)) = split_locator_then_severity(trimmed) else {
            continue;
        };
        match rest {
            Severity::Error => out.error_count += 1,
            Severity::Warning => out.warning_count += 1,
        }
        push_finding(&mut out, loc, trimmed);
    }
    finish(out)
}

/// `a.c:42:5: error: incompatible types`
fn parse_gcc(output: &str) -> ParsedErrors {
    // gcc/clang use the same `loc: severity: message` shape as javac, with a
    // column in the locator.
    parse_javac(output)
}

/// Count `error`/`warning` words per line for toolchains whose format is not
/// recognised. Deliberately conservative: it can only ever *under*-report,
/// never invent a finding.
fn parse_generic(output: &str) -> ParsedErrors {
    let mut out = ParsedErrors::clean();
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        // Word-boundary-ish: skip `0 errors`, `errors: 0`, `no error`.
        if lower.contains("error") && !lower.contains("no error") && !count_is_zero(&lower, "error")
        {
            out.error_count += 1;
            push_finding(&mut out, "", line.trim());
        } else if lower.contains("warning")
            && !lower.contains("no warning")
            && !count_is_zero(&lower, "warning")
        {
            out.warning_count += 1;
            push_finding(&mut out, "", line.trim());
        }
    }
    finish(out)
}

enum Severity {
    Error,
    Warning,
}

/// `a.java:42: error: msg` / `a.c:42:5: error: msg` → `("a.java:42", Error)`.
fn split_locator_then_severity(line: &str) -> Option<(&str, Severity)> {
    // Scan for `: error: ` / `: warning: ` — everything before it is the locator.
    for (marker, sev) in [
        (": error: ", Severity::Error),
        (": warning: ", Severity::Warning),
    ] {
        if let Some(idx) = line.find(marker) {
            return Some((&line[..idx], sev));
        }
    }
    None
}

/// Whether the line reports a zero count for `word` (`0 errors`, `errors: 0`).
fn count_is_zero(lower: &str, word: &str) -> bool {
    lower.contains(&format!("0 {word}"))
        || lower.contains(&format!("{word}: 0"))
        || lower.contains(&format!("{word}s: 0"))
}

fn push_finding(out: &mut ParsedErrors, locator: &str, message: &str) {
    if out.summary.len() >= MAX_SUMMARY_CHARS {
        return;
    }
    let message = message.trim();
    let line = if locator.is_empty() {
        message.to_string()
    } else {
        format!("{locator}  {message}")
    };
    out.summary.push_str(&line);
    out.summary.push('\n');
}

/// Truncate the summary and drop it entirely when nothing was recognised.
fn finish(mut out: ParsedErrors) -> ParsedErrors {
    if out.summary.is_empty() {
        return out;
    }
    if out.summary.len() > MAX_SUMMARY_CHARS {
        // Truncate on a char boundary.
        let mut cut = MAX_SUMMARY_CHARS;
        while cut > 0 && !out.summary.is_char_boundary(cut) {
            cut -= 1;
        }
        out.summary.truncate(cut);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustc_counts_errors_and_warnings_with_locators() {
        let out = parse_errors(
            "cargo check",
            "\
error[E0308]: mismatched types
 --> src/main.rs:42:5
  |
42 |     let x: i32 = \"hello\";
  |            ---   ^^^^^^^ expected `i32`, found `&str`

warning: unused variable: `y`
  --> src/main.rs:10:9

error: aborting due to 1 previous error
",
        );
        assert_eq!(out.error_count, 1, "the `aborting` tally is not a finding");
        assert_eq!(out.warning_count, 1);
        assert!(out.summary.contains("src/main.rs:42:5"), "{}", out.summary);
        assert!(out.summary.contains("mismatched types"), "{}", out.summary);
        assert!(out.summary.contains("src/main.rs:10:9"), "{}", out.summary);
    }

    #[test]
    fn rustc_clean_output_is_empty_not_fabricated() {
        let out = parse_errors("cargo check", "Finished `dev` profile in 0.1s\n");
        assert_eq!(out.error_count, 0);
        assert_eq!(out.warning_count, 0);
        assert!(out.summary.is_empty());
    }

    #[test]
    fn tsc_counts_and_rewrites_the_locator() {
        let out = parse_errors(
            "npx tsc --noEmit",
            "\
src/a.ts(42,5): error TS2322: Type 'string' is not assignable to type 'number'.
src/b.ts(10,1): error TS2304: Cannot find name 'foo'.
src/c.ts(1,1): warning TS6133: 'x' is declared but never used.
",
        );
        assert_eq!(out.error_count, 2);
        assert_eq!(out.warning_count, 1);
        assert!(out.summary.contains("src/a.ts:42:5"), "{}", out.summary);
        assert!(out.summary.contains("TS2322"), "{}", out.summary);
        assert!(out.summary.contains("src/c.ts:1:1"), "{}", out.summary);
    }

    #[test]
    fn javac_and_gcc_count_by_severity_marker() {
        for cmd in ["mvn -q compile", "gcc -fsyntax-only a.c"] {
            let out = parse_errors(
                cmd,
                "a.java:42: error: incompatible types\n\
                 b.c:10:5: warning: unused variable\n",
            );
            assert_eq!(out.error_count, 1, "{cmd}");
            assert_eq!(out.warning_count, 1, "{cmd}");
            assert!(out.summary.contains("a.java:42"), "{cmd}: {}", out.summary);
        }
    }

    #[test]
    fn generic_parser_does_not_invent_findings() {
        // Unknown format: it may under-report, never fabricate.
        let out = parse_errors("make", "cc -c a.c\nall good\n");
        assert_eq!(out.error_count, 0);
        assert_eq!(out.warning_count, 0);
    }

    #[test]
    fn generic_parser_counts_real_lines() {
        let out = parse_errors("make", "a.c:1: error: nope\nb.c:2: warning: hmm\n");
        assert_eq!(out.error_count, 1);
        assert_eq!(out.warning_count, 1);
    }

    #[test]
    fn summary_is_truncated_on_a_char_boundary() {
        let long = "error: x\n".repeat(1000);
        let out = parse_errors("cargo check", &long);
        assert!(out.summary.len() <= MAX_SUMMARY_CHARS);
        assert!(out.summary.is_char_boundary(out.summary.len()));
    }

    #[test]
    fn parse_errors_picks_the_parser_by_command() {
        // The router must not send tsc output to the rustc parser and silently
        // report zero errors.
        let out = parse_errors("npx tsc --noEmit", "src/a.ts(1,1): error TS1: boom\n");
        assert_eq!(out.error_count, 1, "tsc output must route to the tsc parser");
    }
}
