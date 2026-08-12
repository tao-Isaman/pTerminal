//! Auto-update: a once-per-launch check against this repo's public GitHub
//! Releases, and the one-click download that follows. Design doc:
//! `docs/superpowers/specs/2026-08-12-auto-update-installer-design.md`.
//!
//! **No HTTP client dependency.** Both network calls shell out to the
//! `curl.exe` that ships with Windows 10+, as a hidden subprocess — the same
//! `CREATE_NO_WINDOW` pattern `git::git_cmd` already uses. A machine without
//! `curl` (or without network) just never sees an update notice; the check
//! phase is silent by design, because "offline" is a normal state, not an
//! error. Only the user-initiated download phase reports failures.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};

/// GitHub coordinates of the repo whose Releases are the update channel.
/// Fixed when the public repo was created (spec: setup step 1).
const OWNER: &str = "REPLACE_OWNER";
const REPO: &str = "pTerminal";

/// The release asset the installer workflow uploads, and therefore the only
/// thing the updater will ever download and run.
const ASSET_NAME: &str = "pTerminal-setup.exe";

/// A newer published release: the version (without the `v` prefix) and the
/// direct download URL of its installer asset.
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateInfo {
    pub version: String,
    pub installer_url: String,
}

/// Builds a hidden `curl.exe` invocation — without `CREATE_NO_WINDOW` every
/// check would flash a console window, since the release binary is built
/// `windows_subsystem = "windows"` (same reasoning as `git::git_cmd`).
fn curl_cmd(args: &[&str]) -> Command {
    let mut cmd = Command::new("curl");
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// `true` when `remote` is a strictly newer semver than `local`. Both may
/// carry a leading `v`. Anything that doesn't parse as three numeric parts
/// is never "newer" — a malformed tag must not trigger an update notice.
fn is_newer(remote: &str, local: &str) -> bool {
    match (parse_semver(remote), parse_semver(local)) {
        (Some(r), Some(l)) => r > l,
        _ => false,
    }
}

fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Pulls `(tag_name, installer download url)` out of a GitHub
/// `/releases/latest` response body. `None` when the tag is missing or no
/// asset is named [`ASSET_NAME`] — a release without an installer (a
/// source-only tag, a draft mid-upload) must not produce a notice pointing
/// at nothing.
fn parse_release(json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?.to_string();
    let url = v
        .get("assets")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(ASSET_NAME))?
        .get("browser_download_url")?
        .as_str()?
        .to_string();
    Some((tag, url))
}

/// Decides whether `json` (a `/releases/latest` body) announces a version
/// newer than `local_version`. Pure — the testable core of the check.
fn update_from_release(json: &str, local_version: &str) -> Option<UpdateInfo> {
    let (tag, url) = parse_release(json)?;
    if !is_newer(&tag, local_version) {
        return None;
    }
    Some(UpdateInfo {
        version: tag.trim_start_matches('v').to_string(),
        installer_url: url,
    })
}

/// Spawns the once-per-launch update check on a worker thread; the receiver
/// yields at most one [`UpdateInfo`], and only when a newer release with an
/// installer asset exists. Every failure mode (no curl, offline, API rate
/// limit, malformed JSON) is a silent no-op.
pub fn spawn_update_check() -> Receiver<UpdateInfo> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let url =
            format!("https://api.github.com/repos/{OWNER}/{REPO}/releases/latest");
        let Ok(out) = curl_cmd(&["-s", "--max-time", "10", &url]).output() else {
            return;
        };
        if !out.status.success() {
            return;
        }
        let body = String::from_utf8_lossy(&out.stdout);
        if let Some(info) = update_from_release(&body, env!("CARGO_PKG_VERSION")) {
            let _ = tx.send(info); // app may have exited; dropped receiver is fine
        }
    });
    rx
}

/// Downloads the installer to `%TEMP%\pTerminal-setup.exe` on a worker
/// thread. This phase is user-initiated (the update button), so failures
/// are reported for the error dialog rather than swallowed.
pub fn spawn_download(url: String) -> Receiver<Result<PathBuf, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let target = std::env::temp_dir().join(ASSET_NAME);
        let target_str = target.display().to_string();
        let result = match curl_cmd(&[
            "-L", // release assets redirect to a CDN
            "-sS", // quiet progress, keep real errors on stderr
            "--max-time",
            "300",
            "--fail", // 4xx/5xx exit non-zero instead of saving the error page
            "-o",
            &target_str,
            &url,
        ])
        .output()
        {
            Ok(out) if out.status.success() => Ok(target),
            Ok(out) => Err(format!(
                "installer download failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Err(format!("could not run curl: {e}")),
        };
        let _ = tx.send(result); // app may have exited; dropped receiver is fine
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_ordering() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.10", "0.1.9")); // numeric, not lexicographic
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("v0.1.0", "v0.1.0"));
    }

    #[test]
    fn malformed_versions_are_never_newer() {
        assert!(!is_newer("banana", "0.1.0"));
        assert!(!is_newer("1.2", "0.1.0")); // two parts
        assert!(!is_newer("1.2.3.4", "0.1.0")); // four parts
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("9.9.9", "not-a-version"));
    }

    fn release_json(tag: &str, asset_name: &str) -> String {
        format!(
            r#"{{
              "tag_name": "{tag}",
              "name": "release {tag}",
              "assets": [
                {{ "name": "checksums.txt",
                   "browser_download_url": "https://example.com/checksums.txt" }},
                {{ "name": "{asset_name}",
                   "browser_download_url": "https://example.com/{asset_name}" }}
              ]
            }}"#
        )
    }

    #[test]
    fn parses_real_release_shape_and_picks_the_installer_asset() {
        let json = release_json("v0.2.0", "pTerminal-setup.exe");
        assert_eq!(
            parse_release(&json),
            Some((
                "v0.2.0".to_string(),
                "https://example.com/pTerminal-setup.exe".to_string()
            ))
        );
    }

    #[test]
    fn release_without_installer_asset_is_none() {
        let json = release_json("v0.2.0", "pTerminal.zip");
        assert_eq!(parse_release(&json), None);
    }

    #[test]
    fn garbage_json_is_none() {
        assert_eq!(parse_release("not json"), None);
        assert_eq!(parse_release(r#"{"message": "Not Found"}"#), None); // the 404 body
    }

    #[test]
    fn update_from_release_requires_strictly_newer() {
        let json = release_json("v0.2.0", "pTerminal-setup.exe");
        let info = update_from_release(&json, "0.1.0").expect("newer release");
        assert_eq!(info.version, "0.2.0"); // v stripped
        assert_eq!(info.installer_url, "https://example.com/pTerminal-setup.exe");
        assert_eq!(update_from_release(&json, "0.2.0"), None); // same
        assert_eq!(update_from_release(&json, "0.3.0"), None); // local ahead
    }
}
