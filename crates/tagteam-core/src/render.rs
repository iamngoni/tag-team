use std::{fmt::Write as _, time::Duration};

use crate::{Bytes, Check, ResultSummary};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecReport {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub killed: bool,
    pub stdout_tail: Vec<u8>,
    pub stderr_tail: Vec<u8>,
    pub truncated: bool,
    pub took: Duration,
}

impl ExecReport {
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.timed_out {
            let _ = writeln!(out, "timed out after {}", format_duration(self.took));
        } else if self.killed {
            let _ = writeln!(out, "killed after {}", format_duration(self.took));
        } else if let Some(signal) = self.signal {
            let _ = writeln!(out, "terminated by signal {signal}");
        } else {
            let _ = writeln!(out, "exit code {}", self.exit_code.unwrap_or(-1));
        }
        if !self.stdout_tail.is_empty() {
            let _ = writeln!(
                out,
                "\nstdout:\n{}",
                String::from_utf8_lossy(&self.stdout_tail)
            );
        }
        if !self.stderr_tail.is_empty() {
            let _ = writeln!(
                out,
                "\nstderr:\n{}",
                String::from_utf8_lossy(&self.stderr_tail)
            );
        }
        if self.truncated {
            let _ = writeln!(out, "\n[output truncated; tail retained]");
        }
        out
    }
}

#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else if duration.subsec_millis() == 0 {
        format!("{seconds}s")
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

/// Keep the beginning and end of large command output so a model sees both
/// the command context and the final failure.
#[must_use]
pub fn tail_middle(input: &[u8], cap: Bytes) -> (Vec<u8>, bool) {
    let cap = usize::try_from(cap.0).unwrap_or(usize::MAX);
    if input.len() <= cap {
        return (input.to_vec(), false);
    }
    let marker = b"\n[... truncated ...]\n";
    if cap <= marker.len() {
        return (input[input.len().saturating_sub(cap)..].to_vec(), true);
    }
    let remaining = cap - marker.len();
    let head = remaining / 3;
    let tail = remaining - head;
    let mut out = Vec::with_capacity(cap);
    out.extend_from_slice(&input[..head]);
    out.extend_from_slice(marker);
    out.extend_from_slice(&input[input.len() - tail..]);
    (out, true)
}

#[must_use]
pub fn render_checks(checks: &[Check]) -> String {
    let mut out = String::new();
    for check in checks {
        let mark = if check.passed { "PASS" } else { "FAIL" };
        let _ = writeln!(out, "[{mark}] {}", check.what);
        if let Some(command) = &check.command {
            let _ = writeln!(out, "  $ {command}");
        }
        if !check.output_tail.is_empty() {
            let _ = writeln!(out, "  {}", check.output_tail.replace('\n', "\n  "));
        }
    }
    out
}

#[must_use]
pub fn render_summary(result: &ResultSummary) -> String {
    let mut out = format!(
        "{:?}: {}\n{} file(s), +{}, -{}",
        result.outcome,
        result.summary,
        result.diffstat.files,
        result.diffstat.insertions,
        result.diffstat.deletions
    );
    if let Some(branch) = &result.branch {
        let _ = write!(out, "\nbranch: {branch}");
    }
    if !result.assumptions.is_empty() {
        out.push_str("\n\nAssumptions:");
        for assumption in &result.assumptions {
            let _ = write!(out, "\n- {} ({})", assumption.what, assumption.why);
        }
    }
    if !result.checks.is_empty() {
        let _ = write!(out, "\n\nChecks:\n{}", render_checks(&result.checks));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_output_keeps_a_head_and_tail() {
        let input = vec![b'x'; 100];
        let (rendered, truncated) = tail_middle(&input, Bytes(80));
        assert!(truncated);
        assert_eq!(rendered.len(), 80);
        assert!(rendered.windows(5).any(|window| window == b"trunc"));
    }

    #[test]
    fn short_output_is_unchanged() {
        let input = b"ok".to_vec();
        assert_eq!(tail_middle(&input, Bytes(10)), (input, false));
    }
}
