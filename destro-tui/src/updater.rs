//! Self-updater for the `destro` CLI.
//!
//! Releases live on GitHub as bare executables (no archives). This module asks
//! the GitHub API for the newest release, compares it against the version the
//! binary was built from, downloads the matching asset into memory, and swaps
//! it in next to the running executable.
//!
//! Networking only happens inside [`fetch_latest`], [`fetch_latest_from`] and
//! [`update`], so the parsing helpers stay testable without a network.

// The module is declared and wired into the CLI separately, so some of its
// public surface has no caller in the binary yet.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

/// GitHub repository that publishes Destro releases.
pub const REPO: &str = "toeeshchaudhary/destro";

/// Version of the binary currently running.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hard cap for a downloaded executable (128 MiB).
const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;

/// Cap for the release metadata JSON.
const MAX_JSON_BYTES: u64 = 8 * 1024 * 1024;

/// A `major.minor.patch` version. Pre-release / build suffixes are ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// Parse `"v1.2.3"`, `"1.2.3"` or `"1.2.3-beta"`.
    ///
    /// Returns `None` when the string is not three dot-separated numbers.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s
            .strip_prefix('v')
            .or_else(|| s.strip_prefix('V'))
            .unwrap_or(s);
        let core = s.split(['-', '+']).next()?;

        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version(major, minor, patch))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// A parsed GitHub release.
#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    /// `(asset name, browser download URL)`
    pub assets: Vec<(String, String)>,
}

/// Result of an update attempt.
#[derive(Clone, Debug)]
pub enum UpdateOutcome {
    UpToDate {
        current: Version,
        latest: Version,
    },
    Updated {
        from: Version,
        to: Version,
        path: PathBuf,
    },
}

/// Host triple this binary was built for, using the same spelling as the
/// published asset names.
pub fn target_triple() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    return "x86_64-pc-windows-msvc";

    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    return "aarch64-pc-windows-msvc";

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return "x86_64-unknown-linux-gnu";

    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return "aarch64-unknown-linux-gnu";

    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return "x86_64-apple-darwin";

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return "aarch64-apple-darwin";

    #[allow(unreachable_code)]
    "unknown"
}

/// Release asset name for a binary, e.g. `destro-x86_64-pc-windows-msvc.exe`.
pub fn asset_name(binary: &str, triple: &str) -> String {
    if triple.contains("windows") {
        format!("{binary}-{triple}.exe")
    } else {
        format!("{binary}-{triple}")
    }
}

fn user_agent() -> String {
    format!("destro-updater/{CURRENT_VERSION}")
}

/// `GET /repos/{repo}/releases/latest`.
pub fn fetch_latest(repo: &str) -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    fetch_latest_from(&url)
}

/// Fetch a release from an arbitrary GitHub-compatible URL.
pub fn fetch_latest_from(url: &str) -> Result<Release, String> {
    let ua = user_agent();
    let result = ureq::get(url)
        .header("User-Agent", &ua)
        .header("Accept", "application/vnd.github+json")
        .call();

    let mut response = match result {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Err("no releases yet".to_string()),
        Err(ureq::Error::StatusCode(status)) => {
            return Err(format!("release lookup failed: HTTP {status}"));
        }
        Err(err) => return Err(format!("release lookup failed: {err}")),
    };

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("release lookup failed: HTTP {status}"));
    }

    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_JSON_BYTES)
        .read_to_string()
        .map_err(|err| format!("release lookup failed: reading body: {err}"))?;

    parse_release(&body)
}

/// Parse the subset of the GitHub release JSON the updater needs.
pub fn parse_release(json: &str) -> Result<Release, String> {
    if json.trim().is_empty() {
        return Err("release response body is empty".to_string());
    }

    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|err| format!("invalid release JSON: {err}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "invalid release JSON: expected an object".to_string())?;

    let tag = object
        .get("tag_name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .ok_or_else(|| "release JSON has no tag_name".to_string())?;

    let version = Version::parse(tag)
        .ok_or_else(|| format!("release tag {tag:?} is not a semver version"))?;

    let mut assets = Vec::new();
    if let Some(items) = object.get("assets").and_then(|value| value.as_array()) {
        for item in items {
            let name = item.get("name").and_then(|value| value.as_str());
            let url = item
                .get("browser_download_url")
                .and_then(|value| value.as_str());
            if let (Some(name), Some(url)) = (name, url) {
                if name.is_empty() || url.is_empty() {
                    continue;
                }
                assets.push((name.to_string(), url.to_string()));
            }
        }
    }

    Ok(Release {
        tag: tag.to_string(),
        version,
        assets,
    })
}

/// Check for a newer release, download the matching asset and replace the
/// running executable in place.
pub fn update(binary: &str, triple: &str) -> Result<UpdateOutcome, String> {
    update_with(binary, triple, &mut |_, _| {})
}

/// [`update`], reporting download progress as `(bytes so far, total if known)`.
pub fn update_with(
    binary: &str,
    triple: &str,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<UpdateOutcome, String> {
    let current = Version::parse(CURRENT_VERSION).unwrap_or(Version(0, 0, 0));

    let release = fetch_latest(REPO).map_err(|err| format!("check for updates: {err}"))?;
    if release.version <= current {
        return Ok(UpdateOutcome::UpToDate {
            current,
            latest: release.version,
        });
    }

    let wanted = asset_name(binary, triple);
    let url = release
        .assets
        .iter()
        .find(|(name, _)| name == &wanted)
        .map(|(_, url)| url.clone())
        .ok_or_else(|| {
            format!(
                "release {} has no asset named {wanted} (did the build publish it?)",
                release.tag
            )
        })?;

    let bytes = download(&url, progress).map_err(|err| format!("download {wanted}: {err}"))?;

    let target =
        std::env::current_exe().map_err(|err| format!("locate current executable: {err}"))?;
    let staged = with_suffix(&target, ".new");
    fs::write(&staged, &bytes).map_err(|err| format!("write {}: {err}", staged.display()))?;

    swap_in_place(&target, &staged)
        .map_err(|err| format!("replace {}: {err}", target.display()))?;

    Ok(UpdateOutcome::Updated {
        from: current,
        to: release.version,
        path: target,
    })
}

fn download(url: &str, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let ua = user_agent();
    // a slow link is fine; a dead one must not hang forever
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(20)))
        .timeout_global(Some(std::time::Duration::from_secs(45 * 60)))
        .build()
        .into();
    let result = agent.get(url).header("User-Agent", &ua).call();

    let mut response = match result {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Err("asset not found (HTTP 404)".to_string()),
        Err(ureq::Error::StatusCode(status)) => {
            return Err(format!("HTTP {status}"));
        }
        Err(err) => return Err(err.to_string()),
    };

    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}"));
    }
    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let mut reader = response
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD_BYTES)
        .reader();
    let mut out = Vec::with_capacity(total.unwrap_or(0).min(MAX_DOWNLOAD_BYTES) as usize);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|err| format!("reading body: {err}"))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
        progress(out.len() as u64, total);
    }
    if let Some(t) = total
        && out.len() as u64 != t
    {
        return Err(format!("incomplete download ({} of {t} bytes)", out.len()));
    }
    Ok(out)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// Move `staged` over `target`, working around a running executable.
fn swap_in_place(target: &Path, staged: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // A running .exe cannot be overwritten, but it can be renamed.
        let old = with_suffix(target, ".old");
        if old.exists() {
            fs::remove_file(&old)?;
        }
        fs::rename(target, &old)?;
        if let Err(err) = fs::rename(staged, target) {
            // Put the old binary back so the install still runs.
            let _ = fs::rename(&old, target);
            return Err(err);
        }
        Ok(())
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(staged, fs::Permissions::from_mode(0o755))?;
        fs::rename(staged, target)
    }

    #[cfg(not(any(windows, unix)))]
    {
        fs::rename(staged, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "tag_name": "v0.2.0",
        "name": "Destro 0.2.0",
        "draft": false,
        "prerelease": false,
        "assets": [
            {
                "name": "destro-x86_64-pc-windows-msvc.exe",
                "size": 8123456,
                "content_type": "application/octet-stream",
                "browser_download_url": "https://github.com/toeeshchaudhary/destro/releases/download/v0.2.0/destro-x86_64-pc-windows-msvc.exe"
            },
            {
                "name": "destro-harness-x86_64-pc-windows-msvc.exe",
                "size": 9123456,
                "content_type": "application/octet-stream",
                "browser_download_url": "https://github.com/toeeshchaudhary/destro/releases/download/v0.2.0/destro-harness-x86_64-pc-windows-msvc.exe"
            }
        ]
    }"#;

    #[test]
    fn parses_version_forms() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("1.2.3-rc.1"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("V1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse(" 1.2.3 "), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("1.2.3+build.7"), Some(Version(1, 2, 3)));
    }

    #[test]
    fn rejects_junk_versions() {
        for junk in ["", "junk", "v", "1", "1.2", "1.2.3.4", "v1.x.3", "latest"] {
            assert_eq!(Version::parse(junk), None, "expected None for {junk:?}");
        }
    }

    #[test]
    fn orders_versions_numerically() {
        assert!(Version::parse("1.10.0").unwrap() > Version::parse("1.9.9").unwrap());
        assert!(Version::parse("2.0.0").unwrap() > Version::parse("1.99.99").unwrap());
        assert!(Version::parse("1.2.3").unwrap() == Version::parse("v1.2.3-rc.1").unwrap());
        assert!(Version::parse("0.1.0").unwrap() <= Version::parse("0.1.0").unwrap());
    }

    #[test]
    fn displays_versions() {
        assert_eq!(Version(1, 2, 3).to_string(), "1.2.3");
    }

    #[test]
    fn names_assets_per_platform() {
        assert_eq!(
            asset_name("destro", "x86_64-pc-windows-msvc"),
            "destro-x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(
            asset_name("destro", "x86_64-unknown-linux-gnu"),
            "destro-x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            asset_name("destro-harness", "aarch64-apple-darwin"),
            "destro-harness-aarch64-apple-darwin"
        );
    }

    #[test]
    fn target_triple_is_known() {
        assert_ne!(target_triple(), "");
    }

    #[test]
    fn parses_github_release_json() {
        let release = parse_release(FIXTURE).expect("fixture should parse");

        assert_eq!(release.tag, "v0.2.0");
        assert_eq!(release.version, Version(0, 2, 0));
        assert_eq!(release.assets.len(), 2);
        assert_eq!(
            release.assets[0],
            (
                "destro-x86_64-pc-windows-msvc.exe".to_string(),
                "https://github.com/toeeshchaudhary/destro/releases/download/v0.2.0/destro-x86_64-pc-windows-msvc.exe".to_string()
            )
        );
        assert_eq!(
            release.assets[1].0,
            "destro-harness-x86_64-pc-windows-msvc.exe"
        );
        assert!(
            release
                .assets
                .iter()
                .all(|(_, url)| url.starts_with("https://github.com/"))
        );
    }

    #[test]
    fn skips_assets_without_download_url() {
        let json = r#"{
            "tag_name": "v1.0.0",
            "assets": [
                { "name": "kept-x86_64-unknown-linux-gnu", "browser_download_url": "https://example.com/kept" },
                { "name": "no-url-x86_64-unknown-linux-gnu" },
                { "name": "empty-url", "browser_download_url": "" }
            ]
        }"#;
        let release = parse_release(json).expect("should parse");

        assert_eq!(release.version, Version(1, 0, 0));
        assert_eq!(
            release.assets,
            vec![(
                "kept-x86_64-unknown-linux-gnu".to_string(),
                "https://example.com/kept".to_string()
            )]
        );
    }

    #[test]
    fn rejects_bad_release_payloads() {
        assert!(parse_release("").is_err());
        assert!(parse_release("   \n\t ").is_err());
        assert!(parse_release("{not json").is_err());
        assert!(parse_release("[]").is_err());
        assert!(parse_release(r#"{"assets": []}"#).is_err());
        assert!(parse_release(r#"{"tag_name": ""}"#).is_err());
        assert!(parse_release(r#"{"tag_name": "   "}"#).is_err());
        assert!(parse_release(r#"{"tag_name": "nightly"}"#).is_err());
        assert!(parse_release(r#"{"tag_name": 7}"#).is_err());
    }
}
