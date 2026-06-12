//! Sample sourcing: the local cache, and (behind the `au-api` feature)
//! the a-u.supply search engine.
//!
//! All network access happens through `curl` subprocesses on the
//! caller's worker thread — los keeps zero HTTP dependencies and the
//! TUI never blocks on the network. The API key lives in
//! `~/.config/a-u.suppl/env` as `AU_SUPPLY_KEY=…` and is read at call
//! time; no key (or a build without `au-api`) means the module runs
//! local-cache-only and says so in the browser row.
//!
//! Cache layout: `~/.config/los/samples/<id>.wav` (mono, decoded via
//! `afconvert`, which macOS ships) plus `<id>.json` sidecars with the
//! search metadata, so `los samples ls` can say what things are.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One search result, enough to browse and to fetch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Hit {
    pub id: String,
    pub filename: String,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

impl Hit {
    /// Browser row text: name · duration · tags.
    pub fn row(&self) -> String {
        let dur = self
            .duration_seconds
            .map(|d| format!("{d:5.1}s"))
            .unwrap_or_else(|| "    ?s".into());
        let tags = if self.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", self.tags.join(" "))
        };
        format!("{dur}  {}{}", self.filename, tags)
    }
}

pub fn cache_dir() -> PathBuf {
    crate::state::los_dir().join("samples")
}

/// Cached WAV path for a hit (None if not yet downloaded).
pub fn cached(id: &str) -> Option<PathBuf> {
    let p = cache_dir().join(format!("{id}.wav"));
    p.exists().then_some(p)
}

/// Every cached sample, newest first: (wav path, display name).
pub fn list_cache() -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String, std::time::SystemTime)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(cache_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "wav") {
                let name = sidecar_name(&p).unwrap_or_else(|| {
                    p.file_stem().unwrap_or_default().to_string_lossy().into_owned()
                });
                let t = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                out.push((p, name, t));
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.2));
    out.into_iter().map(|(p, n, _)| (p, n)).collect()
}

fn sidecar_name(wav: &Path) -> Option<String> {
    let side = wav.with_extension("json");
    let txt = std::fs::read_to_string(side).ok()?;
    let hit: Hit = serde_json::from_str(&txt).ok()?;
    Some(hit.filename)
}

/// Decode any audio file into a mono f32 reel at `rate`, via
/// `afconvert` (ships with macOS). Capped at `max_secs`.
pub fn load_reel(path: &Path, rate: f32, max_secs: f32) -> Result<super::engine::Reel> {
    // afconvert to a temp wav at the engine rate, 1 channel, LEI16
    let tmp = std::env::temp_dir().join(format!(
        "los-reel-{}.wav",
        std::process::id() as u64 ^ path.as_os_str().len() as u64
    ));
    let status = std::process::Command::new("afconvert")
        .arg(path)
        .arg(&tmp)
        .args(["-f", "WAVE", "-c", "1"])
        .arg("-d")
        .arg(format!("LEI16@{}", rate as u32))
        .status()
        .context("running afconvert (macOS audio converter)")?;
    anyhow::ensure!(status.success(), "afconvert failed for {}", path.display());

    let mut rd = hound::WavReader::open(&tmp).context("reading converted wav")?;
    let max = (max_secs * rate) as usize;
    let data: Vec<f32> = rd
        .samples::<i16>()
        .take(max)
        .map(|s| s.map(|v| v as f32 / 32768.0).unwrap_or(0.0))
        .collect();
    let _ = std::fs::remove_file(&tmp);
    anyhow::ensure!(!data.is_empty(), "no audio in {}", path.display());
    let name = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    Ok(super::engine::Reel { data, name })
}

// ── the API side (feature-gated) ───────────────────────────────────────────

#[cfg(feature = "au-api")]
mod api {
    use super::*;

    const BASE: &str = "https://a-u.supply";

    /// The key, read fresh each call. None = local-only mode.
    pub fn key() -> Option<String> {
        let p = dirs_path();
        let txt = std::fs::read_to_string(p).ok()?;
        txt.lines()
            .filter_map(|l| l.trim().strip_prefix("AU_SUPPLY_KEY="))
            .map(|v| v.trim().trim_matches('"').to_string())
            .next()
            .filter(|v| !v.is_empty())
    }

    fn dirs_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".config/a-u.suppl/env")
    }

    /// Search one of the two pools. `raw = false` → the samples-bored
    /// one-shot index; `raw = true` → un-jobbed input items
    /// (`__inputs__`) with `media_type audio` — the long found reels.
    pub fn search(query: &str, raw: bool, per_page: usize) -> Result<Vec<Hit>> {
        let k = key().context("no AU_SUPPLY_KEY — local cache only")?;
        let filters = if raw {
            serde_json::json!({ "output_index": ["__inputs__"] })
        } else {
            serde_json::json!({ "output_index": ["samples-bored"] })
        };
        let body = serde_json::json!({
            "query": query,
            "media_types": ["audio"],
            "filters": filters,
            "per_page": per_page,
        });
        let out = std::process::Command::new("curl")
            .args(["-s", "--max-time", "15", "-X", "POST"])
            .arg(format!("{BASE}/api/search"))
            .args(["-H", &format!("Authorization: Bearer {k}")])
            .args(["-H", "Content-Type: application/json"])
            .args(["-d", &body.to_string()])
            .output()
            .context("running curl")?;
        anyhow::ensure!(out.status.success(), "curl failed");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .context("parsing search response")?;
        let hits = v
            .get("hits")
            .or_else(|| v.get("results"))
            .and_then(|h| h.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(hits
            .into_iter()
            .filter_map(|h| serde_json::from_value(h).ok())
            .collect())
    }

    /// Download a hit into the cache (id.wav + id.json), converting to
    /// mono WAV at 48 k via afconvert. Returns the cached path.
    pub fn fetch(hit: &Hit) -> Result<PathBuf> {
        if let Some(p) = cached(&hit.id) {
            return Ok(p);
        }
        let k = key().context("no AU_SUPPLY_KEY — local cache only")?;
        std::fs::create_dir_all(cache_dir())?;
        let raw = cache_dir().join(format!("{}.dl", hit.id));
        let status = std::process::Command::new("curl")
            .args(["-s", "--max-time", "120", "-L", "-o"])
            .arg(&raw)
            .args(["-H", &format!("Authorization: Bearer {k}")])
            .arg(format!("{BASE}/api/media/{}/file", hit.id))
            .status()
            .context("running curl download")?;
        anyhow::ensure!(status.success(), "download failed for {}", hit.id);

        let wav = cache_dir().join(format!("{}.wav", hit.id));
        let conv = std::process::Command::new("afconvert")
            .arg(&raw)
            .arg(&wav)
            .args(["-f", "WAVE", "-c", "1", "-d", "LEI16@48000"])
            .status()
            .context("running afconvert")?;
        let _ = std::fs::remove_file(&raw);
        anyhow::ensure!(conv.success(), "could not decode {}", hit.filename);
        let _ = std::fs::write(
            cache_dir().join(format!("{}.json", hit.id)),
            serde_json::to_string_pretty(hit)?,
        );
        Ok(wav)
    }
}

#[cfg(feature = "au-api")]
pub use api::{fetch, key, search};

#[cfg(not(feature = "au-api"))]
pub fn key() -> Option<String> {
    None
}

#[cfg(not(feature = "au-api"))]
pub fn search(_query: &str, _raw: bool, _per_page: usize) -> Result<Vec<Hit>> {
    anyhow::bail!("built without the au-api feature — local cache only")
}

#[cfg(not(feature = "au-api"))]
pub fn fetch(_hit: &Hit) -> Result<PathBuf> {
    anyhow::bail!("built without the au-api feature — local cache only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_rows_read_well() {
        let h = Hit {
            id: "x".into(),
            filename: "kick.wav".into(),
            duration_seconds: Some(0.31),
            tags: vec!["drum".into()],
            description: None,
        };
        let r = h.row();
        assert!(r.contains("kick.wav") && r.contains("0.3s") && r.contains("[drum]"));
    }

    #[test]
    fn cache_listing_survives_missing_dir() {
        // must not panic when the cache doesn't exist yet
        let _ = list_cache();
    }
}
