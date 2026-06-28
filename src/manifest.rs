//! Machine-wide cache manifest (`$cache_dir/manifest.json`).
//!
//! Records, per cached resource (keyed by its path relative to `cache_dir`),
//! how it was placed (`mode`), where it points (`target`), its `size`, and when
//! it was fetched. `tsm doctor` reads this to verify each entry's existence,
//! link liveness, and size against the on-disk artefact (ADR-0008).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::LinkMode;

/// Top-level cache manifest. `resources` is keyed by the resource's path
/// relative to `cache_dir` (e.g. `"models/ruri-v3-30m"`, `"wnjpn.db"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub resources: BTreeMap<String, ManifestEntry>,
}

/// A single cached resource. `model_id` and `source_url` are mutually
/// resource-specific (model vs WordNet) and omitted from JSON when absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// How the cache entry references its upstream (`symlink` / `copy`).
    pub mode: LinkMode,
    /// Upstream the entry points at (HF snapshot dir, or cache-relative source).
    pub target: PathBuf,
    /// Size in bytes (sum of files for a directory). Used by `tsm doctor`.
    pub size: u64,
    /// RFC 3339 timestamp of when the entry was last populated.
    pub fetched_at: String,
    /// Artefact version, enabling version coexistence / rollback (WordNet
    /// entries carry e.g. `"v1.1"`; the model's revision lives in `target`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// HuggingFace model id (model entries only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Origin URL the artefact was downloaded from (WordNet entries only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

/// Read and parse the manifest at `path`.
pub fn read(path: &Path) -> Result<Manifest> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading manifest at {}", path.display()))?;
    let manifest = serde_json::from_str(&raw)
        .with_context(|| format!("parsing manifest at {}", path.display()))?;
    Ok(manifest)
}

/// Serialize `manifest` to `path` as pretty-printed JSON.
pub fn write(path: &Path, manifest: &Manifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating manifest parent dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(manifest).context("serializing manifest")?;
    std::fs::write(path, json)
        .with_context(|| format!("writing manifest to {}", path.display()))?;
    Ok(())
}

/// Resolve a manifest entry `target` to a concrete filesystem path:
/// an absolute target is used as-is (e.g. the model's external HF snapshot dir),
/// a relative one is joined onto `cache_dir` (e.g. `sources/wnjpn-v1.1.db`).
/// Relative storage lets the cache be relocated without rewriting the manifest.
pub fn resolve_target(cache_dir: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        cache_dir.join(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> Manifest {
        let mut resources = BTreeMap::new();
        resources.insert(
            "models/ruri-v3-30m".to_string(),
            ManifestEntry {
                mode: LinkMode::Symlink,
                target: PathBuf::from("/abs/path/to/hf/snapshot"),
                size: 123_456,
                fetched_at: "2026-06-26T15:00:00+09:00".to_string(),
                version: None,
                model_id: Some("cl-nagoya/ruri-v3-30m".to_string()),
                source_url: None,
            },
        );
        resources.insert(
            "wnjpn.db".to_string(),
            ManifestEntry {
                mode: LinkMode::Copy,
                target: PathBuf::from("sources/wnjpn-v1.1.db"),
                size: 213_000_000,
                fetched_at: "2026-06-26T15:00:00+09:00".to_string(),
                version: Some("v1.1".to_string()),
                model_id: None,
                source_url: Some("https://example.com/wnjpn.db.gz".to_string()),
            },
        );
        Manifest {
            version: 1,
            resources,
        }
    }

    #[test]
    fn roundtrip_preserves_contents() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("manifest.json");
        let manifest = sample_manifest();

        write(&path, &manifest).unwrap();
        let read_back = read(&path).unwrap();

        assert_eq!(read_back, manifest);
    }

    #[test]
    fn write_emits_pretty_json_with_expected_shape() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("manifest.json");

        write(&path, &sample_manifest()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();

        // Pretty-printed (indented, multi-line).
        assert!(raw.contains("\n  "), "expected indented JSON, got: {raw}");
        // Map keyed by path, not a Vec / tagged enum.
        assert!(raw.contains("\"models/ruri-v3-30m\""));
        assert!(raw.contains("\"wnjpn.db\""));
        assert!(raw.contains("\"version\": 1"));
        // Resource-specific fields present where they belong.
        assert!(raw.contains("\"model_id\": \"cl-nagoya/ruri-v3-30m\""));
        assert!(raw.contains("\"source_url\""));
        assert!(raw.contains("\"version\": \"v1.1\""));
        // LinkMode serializes lowercase.
        assert!(raw.contains("\"mode\": \"symlink\""));
        assert!(raw.contains("\"mode\": \"copy\""));
    }

    #[test]
    fn omits_none_fields_from_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("manifest.json");
        let mut resources = BTreeMap::new();
        resources.insert(
            "wnjpn.db".to_string(),
            ManifestEntry {
                mode: LinkMode::Symlink,
                target: PathBuf::from("sources/wnjpn-v1.1.db"),
                size: 1,
                fetched_at: "2026-06-26T15:00:00+09:00".to_string(),
                version: None,
                model_id: None,
                source_url: None,
            },
        );
        write(
            &path,
            &Manifest {
                version: 1,
                resources,
            },
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();

        assert!(!raw.contains("model_id"), "None model_id must be omitted");
        assert!(
            !raw.contains("source_url"),
            "None source_url must be omitted"
        );
        // The top-level `"version": 1` stays; only the entry-level `version`
        // (a string) is omitted when None.
        assert!(!raw.contains("\"version\": \"") && !raw.contains("\"version\":\""));
    }

    #[test]
    fn read_missing_file_is_err() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.json");

        assert!(read(&path).is_err());
    }

    #[test]
    fn read_malformed_json_is_err() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("manifest.json");
        std::fs::write(&path, "{ not valid json").unwrap();

        assert!(read(&path).is_err());
    }

    #[test]
    fn resolve_target_keeps_absolute_and_joins_relative() {
        let cache = Path::new("/home/u/.cache/tsm");

        // Relative (wnjpn): joined onto cache_dir.
        assert_eq!(
            resolve_target(cache, Path::new("sources/wnjpn-v1.1.db")),
            PathBuf::from("/home/u/.cache/tsm/sources/wnjpn-v1.1.db")
        );
        // Absolute (model snapshot): used as-is.
        let abs = Path::new("/abs/hf/snapshots/deadbeef");
        assert_eq!(resolve_target(cache, abs), abs.to_path_buf());
    }
}
