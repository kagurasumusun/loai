//! Model catalog: profiles parsed from models.json (embedded at build time).

use crate::config;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HfSource {
    pub repo: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhSource {
    pub repo: String,
    pub tag: String,
    pub parts: Vec<String>,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlamaTuning {
    pub ctx: u32,
    pub kv_quant: Option<String>,
    /// null = auto (all layers to GPU when a GPU backend is used)
    pub ngl: Option<u32>,
    pub fa: bool,
    pub jinja: bool,
    /// hybrid thinking models (qwen3): agent disables thinking for speed by default
    pub hybrid_thinking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub family: String,
    pub desc: String,
    pub tags: Vec<String>,
    pub weights_bytes: u64,
    pub min_ram_gb: u32,
    pub recommended_ram_gb: u32,
    pub experimental: bool,
    /// fp16 KV cache bytes per token (arch-derived)
    pub kv_bytes_per_token: u64,
    pub tuning: LlamaTuning,
    pub hf: HfSource,
    pub gh: Option<GhSource>,
}

#[derive(Debug, Deserialize)]
struct CatalogFile {
    profiles: Vec<Profile>,
}

pub struct Catalog {
    pub profiles: Vec<Profile>,
}

impl Catalog {
    pub fn load() -> Result<Self> {
        // Prefer a local models.json next to the binary (fork-friendly), fall back to embedded.
        let local = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("models.json")))
            .filter(|p| p.exists());
        let text: String = if let Some(p) = local {
            std::fs::read_to_string(&p).with_context(|| format!("{} の読み込みに失敗", p.display()))?
        } else {
            include_str!("../models.json").to_string()
        };
        let cat: CatalogFile = serde_json::from_str(&text).context("models.json のパースに失敗")?;
        Ok(Catalog { profiles: cat.profiles })
    }

    pub fn find(&self, id: &str) -> Result<&Profile> {
        self.profiles
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| anyhow!("不明なモデルID: {id}\n`loai models list` で一覧を確認してください"))
    }

    /// Resolve model: explicit id > config > auto recommendation by RAM.
    pub fn resolve(&self, explicit: Option<&str>) -> Result<&Profile> {
        if let Some(id) = explicit {
            return self.find(id);
        }
        let cfg = crate::config::load_config();
        if let Some(id) = cfg.model {
            if let Ok(p) = self.find(&id) {
                return Ok(p);
            }
        }
        let budget = crate::doctor::model_budget_bytes();
        let fits: Vec<&Profile> = self
            .profiles
            .iter()
            .filter(|p| !p.experimental && p.estimated_memory_bytes(p.tuning.ctx) <= budget)
            .collect();
        let pick = fits
            .iter()
            .max_by_key(|p| p.weights_bytes)
            .copied()
            .unwrap_or_else(|| {
                // fallback: the smallest non-experimental profile
                self.profiles
                    .iter()
                    .filter(|p| !p.experimental)
                    .min_by_key(|p| p.weights_bytes)
                    .unwrap_or(&self.profiles[0])
            });
        Ok(pick)
    }
}

impl Profile {
    /// Estimated total memory (weights + quantized KV + overhead) for a ctx size.
    pub fn estimated_memory_bytes(&self, ctx: u32) -> u64 {
        let kv_div: f64 = match self.tuning.kv_quant.as_deref() {
            Some("q4_0") => 4.0,
            Some("q8_0") => 2.0,
            _ => 1.0,
        };
        let kv = (self.kv_bytes_per_token as f64 * ctx as f64 / kv_div) as u64;
        let overhead: u64 = 350 * 1024 * 1024; // compute buffers etc.
        self.weights_bytes + kv + overhead
    }

    pub fn kv_bytes(&self, ctx: u32) -> u64 {
        let kv_div: f64 = match self.tuning.kv_quant.as_deref() {
            Some("q4_0") => 4.0,
            Some("q8_0") => 2.0,
            _ => 1.0,
        };
        (self.kv_bytes_per_token as f64 * ctx as f64 / kv_div) as u64
    }

    pub fn local_path(&self) -> PathBuf {
        config::models_dir().join(&self.hf.file)
    }

    pub fn is_pulled(&self) -> bool {
        match std::fs::metadata(self.local_path()) {
            Ok(m) => m.len() == self.hf.size,
            Err(_) => false,
        }
    }
}

pub fn hf_url(repo: &str, file: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/main/{file}")
}

pub fn gh_asset_url(repo: &str, tag: &str, asset: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{asset}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_loads() {
        let cat = Catalog::load().unwrap();
        assert!(cat.profiles.len() >= 7, "7 profiles expected");
        assert_eq!(cat.profiles[0].id, "qwen3-8b");
        for p in &cat.profiles {
            assert_eq!(p.hf.size, p.weights_bytes, "{} size mismatch", p.id);
            assert!(p.hf.sha256.len() == 64, "{} bad sha256", p.id);
            assert!(p.kv_bytes_per_token > 0);
        }
    }

    #[test]
    fn test_memory_estimates() {
        let cat = Catalog::load().unwrap();
        let p = cat.find("qwen3-8b").unwrap();
        // weights ~4.7GiB + KV q8 ~1.2GB + 0.35GB overhead
        let m = p.estimated_memory_bytes(16384);
        assert!(m > 5_500_000_000 && m < 7_000_000_000, "estimate out of range: {m}");
    }
}
