//! Optional cron-configuration step used by `drip init` (bd issue
//! drip-01g.5). Cron is the default over a systemd user timer because a
//! non-interactive service account (e.g. the eventual syncthing user) may
//! not have `loginctl enable-linger` set up for user timers to run without
//! an active login session.
//!
//! Split into pure logic (unit-testable, no I/O) and thin shell-out
//! wrappers, per this repo's convention of keeping vault/system-touching
//! code separate from testable logic -- see `src/digest.rs`/`src/journal.rs`.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

/// Marks the comment line immediately preceding a drip-managed cron entry,
/// so a re-run of `drip init`'s cron step can find and replace its own
/// previously-installed line instead of duplicating it.
pub const MARKER: &str = "# drip fetch (managed by `drip init`)";

/// Parse a 24-hour "HH:MM" time string into `(hour, minute)`.
pub fn parse_time(input: &str) -> Result<(u32, u32)> {
    let input = input.trim();
    let (hour_str, minute_str) = input
        .split_once(':')
        .with_context(|| format!("'{input}' is not a valid HH:MM time"))?;

    let hour: u32 = hour_str
        .trim()
        .parse()
        .with_context(|| format!("'{input}' is not a valid HH:MM time"))?;
    let minute: u32 = minute_str
        .trim()
        .parse()
        .with_context(|| format!("'{input}' is not a valid HH:MM time"))?;

    if hour > 23 {
        bail!("hour must be between 0 and 23, got {hour}");
    }
    if minute > 59 {
        bail!("minute must be between 0 and 59, got {minute}");
    }

    Ok((hour, minute))
}

/// Render the marker comment followed by the actual cron line, matching
/// the README's existing style (`README.md`'s "Running unattended" section).
pub fn build_line(
    hour: u32,
    minute: u32,
    binary_path: &str,
    fetch_args: &str,
    log_path: &str,
) -> String {
    format!("{MARKER}\n{minute} {hour} * * * {binary_path} fetch {fetch_args} >> {log_path} 2>&1")
}

/// Merge `new_block` (a marker line followed by a cron line, as produced by
/// [`build_line`]) into `existing_crontab`. Pure function, no I/O.
///
/// If `existing_crontab` already contains a line equal to `marker`, that
/// line and the line immediately after it (the previously-installed drip
/// cron line) are replaced with `new_block`. Otherwise `new_block` is
/// appended to the end, with exactly one blank-line separation and no
/// double trailing blank line.
pub fn upsert_line(existing_crontab: &str, marker: &str, new_block: &str) -> String {
    let lines: Vec<&str> = existing_crontab.lines().collect();

    if let Some(marker_idx) = lines.iter().position(|line| *line == marker) {
        let mut result: Vec<&str> = Vec::with_capacity(lines.len());
        result.extend_from_slice(&lines[..marker_idx]);
        for new_line in new_block.lines() {
            result.push(new_line);
        }
        // Skip the marker line and, if present, the cron line right after
        // it -- that's the previously-installed drip entry being replaced.
        let skip_to = if marker_idx + 1 < lines.len() {
            marker_idx + 2
        } else {
            marker_idx + 1
        };
        result.extend_from_slice(&lines[skip_to.min(lines.len())..]);

        let mut out = result.join("\n");
        out.push('\n');
        out
    } else {
        let trimmed = existing_crontab.trim_end_matches('\n');
        if trimmed.is_empty() {
            let mut out = new_block.to_string();
            out.push('\n');
            out
        } else {
            let mut out = trimmed.to_string();
            out.push('\n');
            out.push_str(new_block);
            out.push('\n');
            out
        }
    }
}

/// Read the current user's crontab via `crontab -l`. A missing crontab
/// (cron's "no crontab for user" case, a non-zero exit) is treated as an
/// empty crontab rather than an error; any other failure is propagated.
pub fn read_crontab() -> Result<String> {
    let output = Command::new("crontab")
        .arg("-l")
        .output()
        .context("failed to run `crontab -l`")?;

    if output.status.success() {
        return String::from_utf8(output.stdout).context("`crontab -l` output was not valid UTF-8");
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    if stderr.contains("no crontab") {
        return Ok(String::new());
    }

    bail!("`crontab -l` failed: {}", stderr.trim());
}

/// Write `contents` as the current user's full crontab via `crontab -`.
pub fn write_crontab(contents: &str) -> Result<()> {
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to run `crontab -`")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .context("failed to open stdin for `crontab -`")?;
        stdin
            .write_all(contents.as_bytes())
            .context("failed to write to `crontab -`'s stdin")?;
    }

    let status = child.wait().context("failed to wait on `crontab -`")?;
    if !status.success() {
        bail!("`crontab -` exited with a non-zero status");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_time_accepts_valid_hh_mm() {
        assert_eq!(parse_time("08:00").unwrap(), (8, 0));
        assert_eq!(parse_time("23:59").unwrap(), (23, 59));
        assert_eq!(parse_time("0:0").unwrap(), (0, 0));
    }

    #[test]
    fn parse_time_trims_whitespace() {
        assert_eq!(parse_time("  08:30  ").unwrap(), (8, 30));
    }

    #[test]
    fn parse_time_rejects_out_of_range_hour() {
        assert!(parse_time("24:00").is_err());
    }

    #[test]
    fn parse_time_rejects_out_of_range_minute() {
        assert!(parse_time("08:60").is_err());
    }

    #[test]
    fn parse_time_rejects_unparsable_input() {
        assert!(parse_time("not-a-time").is_err());
        assert!(parse_time("08").is_err());
        assert!(parse_time("08:ab").is_err());
    }

    #[test]
    fn build_line_matches_readme_style() {
        let line = build_line(
            8,
            0,
            "/path/to/drip",
            "--profile weekly-rust",
            "~/.local/log/drip.log",
        );
        assert_eq!(
            line,
            format!(
                "{MARKER}\n0 8 * * * /path/to/drip fetch --profile weekly-rust >> ~/.local/log/drip.log 2>&1"
            )
        );
    }

    #[test]
    fn upsert_line_appends_to_empty_crontab() {
        let new_block = build_line(
            8,
            0,
            "/path/to/drip",
            "--profile weekly-rust",
            "~/.local/log/drip.log",
        );
        let result = upsert_line("", MARKER, &new_block);
        assert_eq!(result, format!("{new_block}\n"));
    }

    #[test]
    fn upsert_line_appends_to_crontab_with_unrelated_lines_only() {
        let existing = "# some other cron job\n0 5 * * * /usr/bin/backup.sh\n";
        let new_block = build_line(
            8,
            0,
            "/path/to/drip",
            "--profile weekly-rust",
            "~/.local/log/drip.log",
        );
        let result = upsert_line(existing, MARKER, &new_block);

        assert_eq!(
            result,
            format!("# some other cron job\n0 5 * * * /usr/bin/backup.sh\n{new_block}\n")
        );
    }

    #[test]
    fn upsert_line_replaces_prior_drip_entry_at_end() {
        let old_block = build_line(
            8,
            0,
            "/path/to/drip",
            "--profile weekly-rust",
            "~/.local/log/drip.log",
        );
        let existing =
            format!("# some other cron job\n0 5 * * * /usr/bin/backup.sh\n{old_block}\n");

        let new_block = build_line(
            9,
            30,
            "/path/to/drip",
            "-s rust,programming",
            "~/.local/log/drip.log",
        );
        let result = upsert_line(&existing, MARKER, &new_block);

        assert_eq!(
            result,
            format!("# some other cron job\n0 5 * * * /usr/bin/backup.sh\n{new_block}\n")
        );
    }

    #[test]
    fn upsert_line_replaces_prior_drip_entry_not_at_end() {
        let old_block = build_line(
            8,
            0,
            "/path/to/drip",
            "--profile weekly-rust",
            "~/.local/log/drip.log",
        );
        let existing =
            format!("{old_block}\n# some other cron job\n0 5 * * * /usr/bin/backup.sh\n");

        let new_block = build_line(
            9,
            30,
            "/path/to/drip",
            "-s rust,programming",
            "~/.local/log/drip.log",
        );
        let result = upsert_line(&existing, MARKER, &new_block);

        assert_eq!(
            result,
            format!("{new_block}\n# some other cron job\n0 5 * * * /usr/bin/backup.sh\n")
        );
    }

    #[test]
    fn upsert_line_handles_marker_as_last_line_with_no_cron_line_after() {
        // Defensive case: marker present but truncated/malformed (no cron
        // line follows it) -- should not panic, should just replace the
        // marker line itself and append the new block after it.
        let existing = format!("# some other cron job\n0 5 * * * /usr/bin/backup.sh\n{MARKER}\n");
        let new_block = build_line(9, 30, "/path/to/drip", "-s rust", "~/.local/log/drip.log");
        let result = upsert_line(&existing, MARKER, &new_block);

        assert_eq!(
            result,
            format!("# some other cron job\n0 5 * * * /usr/bin/backup.sh\n{new_block}\n")
        );
    }

    #[test]
    fn upsert_line_no_double_trailing_blank_line() {
        let existing = "# some other cron job\n0 5 * * * /usr/bin/backup.sh\n";
        let new_block = build_line(
            8,
            0,
            "/path/to/drip",
            "--profile weekly-rust",
            "~/.local/log/drip.log",
        );
        let result = upsert_line(existing, MARKER, &new_block);

        assert!(!result.ends_with("\n\n"));
    }
}
