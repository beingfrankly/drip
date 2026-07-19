//! `drip update` self-update support: checks GitHub's Releases API for a
//! newer tagged release than the running binary and, if found and
//! confirmed, downloads and installs it in place.
//!
//! Deliberately uses no new dependencies -- `reqwest`/`serde`/`serde_json`
//! are already used elsewhere in this codebase, and archive extraction
//! shells out to a system tool (`tar` on Unix, PowerShell's
//! `Expand-Archive` on Windows) rather than adding an archive crate,
//! matching `src/cron.rs`'s precedent of shelling out to a system tool.
//! Releases are now produced by cargo-dist (see
//! `.github/workflows/release.yml`), which publishes per-target archives
//! with no version string in the filename: `.tar.xz` for Unix targets
//! (binary inside a `drip-<triple>/` subdirectory) and `.zip` for Windows
//! (binary at the archive root).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;

use crate::vprintln;

/// The GitHub `owner/repo` this binary is released from.
pub const REPO: &str = "beingfrankly/drip";

/// Base URL for GitHub's REST API. Taken as a function parameter by
/// [`fetch_latest_release`] rather than being hardcoded inside it, so tests
/// can point it at a mock server instead.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// The subset of GitHub's "get the latest release" response this module
/// actually needs. Unknown fields in the real response (there are many --
/// `id`, `body`, `published_at`, etc.) are silently ignored by serde's
/// default behavior; no `#[serde(deny_unknown_fields)]` here on purpose.
#[derive(Debug, Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    pub assets: Vec<GithubAsset>,
}

/// A single release asset (a file attached to a GitHub release).
#[derive(Debug, Deserialize)]
pub struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// GET `{api_base}/repos/{repo}/releases/latest` and parse it into a
/// [`GithubRelease`]. GitHub's API rejects requests with no `User-Agent`
/// header, so one is always sent (`drip/{CARGO_PKG_VERSION}`), alongside an
/// `Accept: application/vnd.github+json` header.
pub fn fetch_latest_release(api_base: &str, repo: &str, verbose: bool) -> Result<GithubRelease> {
    let url = format!("{api_base}/repos/{repo}/releases/latest");

    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client for GitHub releases check")?;

    vprintln(verbose, format!("GET {url}"));

    let user_agent = format!("drip/{}", env!("CARGO_PKG_VERSION"));
    let resp = http
        .get(&url)
        .header(USER_AGENT, user_agent)
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .with_context(|| format!("failed to fetch {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        bail!("failed to fetch latest release from {url}: HTTP {status}");
    }

    let body = resp
        .text()
        .with_context(|| format!("failed to read response body from {url}"))?;

    serde_json::from_str(&body)
        .with_context(|| format!("failed to parse GitHub release JSON from {url}"))
}

/// Parse a bare `"MAJOR.MINOR.PATCH"` version string (no leading `v`) into a
/// `(major, minor, patch)` tuple. Returns `None` on any malformed input --
/// this is a "just tell me if it's newer" helper, not a validator.
pub fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let major = parts[0].parse().ok()?;
    let minor = parts[1].parse().ok()?;
    let patch = parts[2].parse().ok()?;

    Some((major, minor, patch))
}

/// Whether `latest_tag` (a GitHub tag, e.g. `"v0.1.1"`) is newer than
/// `current` (a bare version, e.g. `"0.1.0"`, always
/// `env!("CARGO_PKG_VERSION")` at the call site).
///
/// A single leading `v`/`V` is stripped from `latest_tag` before parsing. If
/// EITHER version fails to parse, this returns `true` -- fail open toward
/// "there might be an update" rather than silently claiming up-to-date on a
/// version string we can't understand. This is a deliberate design choice,
/// not a bug.
pub fn is_newer(current: &str, latest_tag: &str) -> bool {
    let stripped = latest_tag
        .strip_prefix('v')
        .or_else(|| latest_tag.strip_prefix('V'))
        .unwrap_or(latest_tag);

    let parsed_current = parse_version(current);
    let parsed_latest = parse_version(stripped);

    match (parsed_current, parsed_latest) {
        (Some(current), Some(latest)) => latest > current,
        _ => true,
    }
}

/// The cargo-dist release-asset filename for a given (os, arch), matching
/// `.github/workflows/release.yml` exactly. `os`/`arch` use the same values
/// as `std::env::consts::OS`/`ARCH`. Returns `None` for a platform drip does
/// not publish a prebuilt binary for. Note: unlike the old hand-rolled
/// naming scheme, cargo-dist asset names carry NO version string.
pub fn asset_name_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("drip-x86_64-unknown-linux-gnu.tar.xz"),
        ("macos", "x86_64") => Some("drip-x86_64-apple-darwin.tar.xz"),
        ("macos", "aarch64") => Some("drip-aarch64-apple-darwin.tar.xz"),
        ("windows", "x86_64") => Some("drip-x86_64-pc-windows-msvc.zip"),
        _ => None,
    }
}

/// The asset name for the platform THIS binary was built for.
pub fn expected_asset_name() -> Option<&'static str> {
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Find the asset in `release.assets` whose `name` exactly equals
/// `expected_name`, if any.
pub fn find_asset<'a>(release: &'a GithubRelease, expected_name: &str) -> Option<&'a GithubAsset> {
    release.assets.iter().find(|asset| asset.name == expected_name)
}

/// Download the file at `url` to `dest` via a blocking GET. Response bodies
/// here are small binaries (a few MB), so this reads the whole body into
/// memory (`resp.bytes()`) rather than true streaming.
pub fn download_asset(url: &str, dest: &Path, verbose: bool) -> Result<()> {
    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("failed to build HTTP client for release asset download")?;

    vprintln(verbose, format!("GET {url}"));

    let user_agent = format!("drip/{}", env!("CARGO_PKG_VERSION"));
    let resp = http
        .get(url)
        .header(USER_AGENT, user_agent)
        .send()
        .with_context(|| format!("failed to download {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        bail!("failed to download {url}: HTTP {status}");
    }

    let bytes = resp
        .bytes()
        .with_context(|| format!("failed to read response body from {url}"))?;

    std::fs::write(dest, &bytes)
        .with_context(|| format!("failed to write downloaded asset to {}", dest.display()))?;

    Ok(())
}

/// The binary filename inside a release archive for the current platform:
/// `drip.exe` on Windows, `drip` elsewhere.
fn archive_binary_name() -> &'static str {
    if cfg!(windows) {
        "drip.exe"
    } else {
        "drip"
    }
}

/// Recursively search `dir` for a file literally named `name`, returning the
/// first match found. The trees this searches are tiny (a handful of files
/// from a single release archive), so a plain recursive walk is fine -- no
/// need for a directory-walking crate.
fn find_file_named(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.file_name().and_then(|f| f.to_str()) == Some(name) {
            return Some(path);
        }
    }

    for subdir in subdirs {
        if let Some(found) = find_file_named(&subdir, name) {
            return Some(found);
        }
    }

    None
}

/// Extract the `drip` binary out of the release archive at `archive_path`
/// into `dest_dir`, and return the path to the extracted binary.
///
/// cargo-dist's archive layout differs by platform: Unix `.tar.xz` archives
/// put the binary inside a `drip-<triple>/` subdirectory, while the Windows
/// `.zip` puts `drip.exe` at the archive root. Rather than assume either
/// layout, the binary is located by name (`archive_binary_name`) anywhere
/// under `dest_dir` after extraction via [`find_file_named`].
#[cfg(unix)]
pub fn extract_binary(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf> {
    let output = Command::new("tar")
        .arg("-xf")
        .arg(archive_path)
        .arg("-C")
        .arg(dest_dir)
        .output()
        .with_context(|| {
            format!(
                "failed to run `tar -xf {} -C {}`",
                archive_path.display(),
                dest_dir.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`tar -xf {} -C {}` exited with a non-zero status ({}): {}",
            archive_path.display(),
            dest_dir.display(),
            output.status,
            stderr.trim()
        );
    }

    let name = archive_binary_name();
    find_file_named(dest_dir, name).ok_or_else(|| {
        anyhow::anyhow!(
            "could not find a file named '{name}' anywhere under {} after extracting {}",
            dest_dir.display(),
            archive_path.display()
        )
    })
}

/// Windows counterpart of [`extract_binary`]: extracts a `.zip` archive via
/// PowerShell's `Expand-Archive` (no archive crate dependency, matching this
/// module's "shell out to a system tool" convention), then locates
/// `drip.exe` under `dest_dir` the same way the Unix branch does.
#[cfg(windows)]
pub fn extract_binary(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf> {
    let command = format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        archive_path.display(),
        dest_dir.display()
    );

    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .output()
        .with_context(|| format!("failed to run PowerShell Expand-Archive: {command}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "PowerShell Expand-Archive exited with a non-zero status ({}): {}",
            output.status,
            stderr.trim()
        );
    }

    let name = archive_binary_name();
    find_file_named(dest_dir, name).ok_or_else(|| {
        anyhow::anyhow!(
            "could not find a file named '{name}' anywhere under {} after extracting {}",
            dest_dir.display(),
            archive_path.display()
        )
    })
}

/// Safely replace the running binary at `current_exe` with the contents of
/// `new_binary`.
///
/// Copies `new_binary` to a temp file in the SAME DIRECTORY as `current_exe`
/// (so the final `rename` is on the same filesystem -- `std::fs::rename` is
/// only atomic/guaranteed-to-work across paths on the same filesystem;
/// crossing a mount boundary can fail with `EXDEV`), marks it executable,
/// then renames it over `current_exe`. Renaming over a running executable is
/// safe on Linux -- the kernel keeps the old inode alive for the still-
/// running process.
///
/// On any failure, the temp file is best-effort cleaned up before the error
/// is returned.
#[cfg(unix)]
pub fn install_binary(new_binary: &Path, current_exe: &Path) -> Result<()> {
    let temp_path = current_exe.with_extension("new");

    if let Err(err) = std::fs::copy(new_binary, &temp_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err).with_context(|| {
            format!(
                "failed to copy new binary to {} -- if this is a permissions error, re-run with \
                 elevated permissions or reinstall manually",
                temp_path.display()
            )
        });
    }

    if let Err(err) = std::fs::set_permissions(
        &temp_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    ) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err).with_context(|| {
            format!("failed to mark {} as executable", temp_path.display())
        });
    }

    if let Err(err) = std::fs::rename(&temp_path, current_exe) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err).with_context(|| {
            format!(
                "failed to install the new binary over {} -- if this is a permissions error, \
                 re-run with elevated permissions or reinstall manually",
                current_exe.display()
            )
        });
    }

    Ok(())
}

/// Windows counterpart of [`install_binary`]. Windows refuses to overwrite
/// (rename over or truncate) an `.exe` file that's currently running, so the
/// approach differs from the Unix branch: the running `current_exe` is
/// first renamed to a sidelined `.old` path -- the running process keeps
/// executing fine from that renamed file, Windows just won't let it be
/// deleted or replaced by name anymore -- freeing up `current_exe`'s
/// original name, and then the new binary is copied into that freed name.
/// The sidelined `.old` file is best-effort removed afterward; while the
/// old process is still running this removal will typically fail with a
/// sharing violation, which is expected and ignored.
///
/// On any failure after the rename, best-effort attempts to restore the
/// original binary before returning the error.
#[cfg(windows)]
pub fn install_binary(new_binary: &Path, current_exe: &Path) -> Result<()> {
    let sidelined_path = current_exe.with_extension("old.exe");

    // Best-effort: an `.old.exe` left over from a previous update attempt
    // may still be present and would make this rename fail.
    let _ = std::fs::remove_file(&sidelined_path);

    std::fs::rename(current_exe, &sidelined_path).with_context(|| {
        format!(
            "failed to sideline the running binary from {} to {} -- if this is a permissions \
             error, re-run with elevated permissions or reinstall manually",
            current_exe.display(),
            sidelined_path.display()
        )
    })?;

    if let Err(err) = std::fs::copy(new_binary, current_exe) {
        // Best-effort restore so the install isn't left in a broken state.
        let _ = std::fs::rename(&sidelined_path, current_exe);
        return Err(err).with_context(|| {
            format!(
                "failed to install the new binary to {} -- if this is a permissions error, \
                 re-run with elevated permissions or reinstall manually",
                current_exe.display()
            )
        });
    }

    // Best-effort cleanup: while the old (still-running) process holds this
    // file open, removal will typically fail with a sharing violation --
    // that's expected and not a failure of the update itself.
    let _ = std::fs::remove_file(&sidelined_path);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_accepts_valid_major_minor_patch() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
    }

    #[test]
    fn parse_version_rejects_malformed_input() {
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.x"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn is_newer_true_when_tag_is_newer() {
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("0.1.0", "v0.2.0"));
        assert!(is_newer("0.1.0", "v1.0.0"));
    }

    #[test]
    fn is_newer_false_when_tag_is_older() {
        assert!(!is_newer("0.2.0", "v0.1.0"));
    }

    #[test]
    fn is_newer_false_when_tag_is_equal() {
        assert!(!is_newer("0.1.0", "v0.1.0"));
    }

    #[test]
    fn is_newer_fails_open_on_malformed_current() {
        assert!(is_newer("not-a-version", "v0.1.0"));
    }

    #[test]
    fn is_newer_fails_open_on_malformed_tag() {
        assert!(is_newer("0.1.0", "not-a-version"));
    }

    #[test]
    fn is_newer_strips_leading_v_or_uppercase_v() {
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("0.1.0", "V0.1.1"));
    }

    #[test]
    fn asset_name_for_matches_cargo_dist_naming() {
        assert_eq!(
            asset_name_for("linux", "x86_64"),
            Some("drip-x86_64-unknown-linux-gnu.tar.xz")
        );
        assert_eq!(
            asset_name_for("macos", "x86_64"),
            Some("drip-x86_64-apple-darwin.tar.xz")
        );
        assert_eq!(
            asset_name_for("macos", "aarch64"),
            Some("drip-aarch64-apple-darwin.tar.xz")
        );
        assert_eq!(
            asset_name_for("windows", "x86_64"),
            Some("drip-x86_64-pc-windows-msvc.zip")
        );

        assert_eq!(asset_name_for("linux", "aarch64"), None);
        assert_eq!(asset_name_for("windows", "aarch64"), None);
        assert_eq!(asset_name_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn find_asset_finds_the_matching_asset_among_several() {
        let release = GithubRelease {
            tag_name: "v0.1.0".to_string(),
            assets: vec![
                GithubAsset {
                    name: "drip-x86_64-unknown-linux-gnu.tar.xz".to_string(),
                    browser_download_url: "https://example.com/a".to_string(),
                },
                GithubAsset {
                    name: "drip-aarch64-apple-darwin.tar.xz".to_string(),
                    browser_download_url: "https://example.com/b".to_string(),
                },
            ],
        };

        let found = find_asset(&release, "drip-x86_64-unknown-linux-gnu.tar.xz")
            .expect("should find the matching asset");
        assert_eq!(found.browser_download_url, "https://example.com/a");
    }

    #[test]
    fn find_asset_returns_none_when_absent() {
        let release = GithubRelease {
            tag_name: "v0.1.0".to_string(),
            assets: vec![GithubAsset {
                name: "drip-aarch64-apple-darwin.tar.xz".to_string(),
                browser_download_url: "https://example.com/b".to_string(),
            }],
        };

        assert!(find_asset(&release, "drip-x86_64-unknown-linux-gnu.tar.xz").is_none());
    }

    #[test]
    fn fetch_latest_release_parses_a_real_shaped_response() {
        let mut server = mockito::Server::new();
        let body = r#"{
            "url": "https://api.github.com/repos/beingfrankly/drip/releases/12345",
            "id": 12345,
            "tag_name": "v0.1.0",
            "name": "v0.1.0",
            "published_at": "2026-07-01T00:00:00Z",
            "body": "Initial release",
            "assets": [
                {
                    "name": "drip-x86_64-unknown-linux-gnu.tar.xz",
                    "browser_download_url": "https://github.com/beingfrankly/drip/releases/download/v0.1.0/drip-x86_64-unknown-linux-gnu.tar.xz",
                    "size": 1234567,
                    "id": 999
                }
            ]
        }"#;
        let _mock = server
            .mock("GET", "/repos/beingfrankly/drip/releases/latest")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create();

        let release = fetch_latest_release(&server.url(), "beingfrankly/drip", false)
            .expect("fetch should succeed against the mock server");

        assert_eq!(release.tag_name, "v0.1.0");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].name, "drip-x86_64-unknown-linux-gnu.tar.xz");
        assert_eq!(
            release.assets[0].browser_download_url,
            "https://github.com/beingfrankly/drip/releases/download/v0.1.0/drip-x86_64-unknown-linux-gnu.tar.xz"
        );
    }

    #[test]
    fn fetch_latest_release_errors_clearly_on_non_2xx_status() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/repos/beingfrankly/drip/releases/latest")
            .with_status(404)
            .with_body("not found")
            .create();

        let err = fetch_latest_release(&server.url(), "beingfrankly/drip", false)
            .expect_err("a 404 response should produce an error");

        assert!(
            err.to_string().contains("404"),
            "error should mention the HTTP status: {err}"
        );
    }

    #[test]
    fn download_asset_writes_the_response_body_to_dest() {
        let mut server = mockito::Server::new();
        let expected = b"pretend this is a tarball's bytes";
        let _mock = server
            .mock("GET", "/asset.tar.gz")
            .with_status(200)
            .with_body(expected)
            .create();

        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("asset.tar.gz");
        let url = format!("{}/asset.tar.gz", server.url());

        download_asset(&url, &dest, false).expect("download should succeed");

        let written = std::fs::read(&dest).expect("dest file should have been written");
        assert_eq!(written, expected);
    }

    #[test]
    fn download_asset_errors_clearly_on_non_2xx_status() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/asset.tar.gz")
            .with_status(500)
            .with_body("server error")
            .create();

        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("asset.tar.gz");
        let url = format!("{}/asset.tar.gz", server.url());

        let err = download_asset(&url, &dest, false).expect_err("a 500 response should error");
        assert!(
            err.to_string().contains("500"),
            "error should mention the HTTP status: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_binary_extracts_the_drip_file_from_a_real_tar_xz_fixture() {
        // Mirrors the real cargo-dist layout: a top-level `drip-<triple>/`
        // directory containing `drip` (and, in real releases, a README --
        // irrelevant to this test).
        let src_dir = tempfile::tempdir().expect("tempdir for archive contents");
        let subdir_name = "drip-x86_64-unknown-linux-gnu";
        let subdir = src_dir.path().join(subdir_name);
        std::fs::create_dir(&subdir).expect("create fixture subdir");

        let binary_content = b"fake drip binary bytes";
        let binary_path = subdir.join("drip");
        std::fs::write(&binary_path, binary_content).expect("write fake binary");

        let archive_dir = tempfile::tempdir().expect("tempdir for archive itself");
        let archive_path = archive_dir.path().join("drip-x86_64-unknown-linux-gnu.tar.xz");

        let status = Command::new("tar")
            .arg("-cJf")
            .arg(&archive_path)
            .arg("-C")
            .arg(src_dir.path())
            .arg(subdir_name)
            .status()
            .expect("failed to run tar to build the test fixture");
        assert!(status.success(), "tar -cJf should succeed building the fixture");

        let dest_dir = tempfile::tempdir().expect("tempdir for extraction destination");
        let extracted = extract_binary(&archive_path, dest_dir.path())
            .expect("extract_binary should succeed");

        assert_eq!(extracted, dest_dir.path().join(subdir_name).join("drip"));
        assert_eq!(extracted.file_name().unwrap(), "drip");
        let content = std::fs::read(&extracted).expect("extracted file should be readable");
        assert_eq!(content, binary_content);
    }

    #[test]
    fn install_binary_replaces_current_exe_content_and_marks_it_executable() {
        let dir = tempfile::tempdir().expect("tempdir");

        let current_exe = dir.path().join("drip");
        std::fs::write(&current_exe, b"old binary content").expect("write fake current exe");

        let new_binary = dir.path().join("drip-new-download");
        std::fs::write(&new_binary, b"new binary content").expect("write fake new binary");

        install_binary(&new_binary, &current_exe).expect("install_binary should succeed");

        let content = std::fs::read(&current_exe).expect("current_exe should be readable");
        assert_eq!(content, b"new binary content");

        // The executable bit is a POSIX concept; Windows has no equivalent
        // (a `.exe` is executable by extension alone), so only assert it on
        // Unix -- see `install_binary`'s own `#[cfg(unix)]` chmod step.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&current_exe)
                .expect("metadata should be readable")
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "installed binary should be executable");
        }
    }
}
