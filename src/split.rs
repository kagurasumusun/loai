//! Split large GGUF files into <2GB chunks for GitHub Releases mirroring,
//! and join them back with sha256 verification.

use crate::download::sha256_file;
use crate::tui;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_CHUNK_BYTES: u64 = 1900 * 1024 * 1024; // stay under GitHub's 2GiB asset limit

#[derive(Serialize, Deserialize)]
pub struct PartInfo {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize)]
pub struct SplitManifest {
    pub file: String,
    pub size: u64,
    pub sha256: String,
    pub chunk_bytes: u64,
    pub parts: Vec<PartInfo>,
}

pub fn manifest_path_for(file: &Path) -> PathBuf {
    let mut name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    name.push_str(".loai-manifest.json");
    file.with_file_name(name)
}

/// Split `file` into parts and write a manifest next to it.
pub fn split_file(file: &Path, chunk_bytes: u64) -> Result<SplitManifest> {
    let meta = fs::metadata(file).with_context(|| format!("ファイルがありません: {}", file.display()))?;
    let total = meta.len();
    tui::info(&format!("sha256 を計算中: {}", file.display()));
    let full_sha = sha256_file(file)?;

    let fname = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let n_parts = (total + chunk_bytes - 1) / chunk_bytes;
    tui::info(&format!("{} を {} 個に分割します (1パート約 {})", fname, n_parts, crate::util::human_bytes(chunk_bytes)));

    let mut src = File::open(file)?;
    let mut parts = Vec::new();
    let mut remaining = total;
    let mut idx = 0u32;
    while remaining > 0 {
        let this_chunk = chunk_bytes.min(remaining);
        let part_name = format!("{fname}.part-{idx:03}");
        let part_path = file.with_file_name(&part_name);
        let mut out = File::create(&part_path)?;
        let mut written = 0u64;
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        while written < this_chunk {
            let want = (this_chunk - written).min(buf.len() as u64) as usize;
            let n = src.read(&mut buf[..want])?;
            if n == 0 {
                return Err(anyhow!("予期しないEOF"));
            }
            out.write_all(&buf[..n])?;
            written += n as u64;
        }
        out.flush()?;
        let sha = sha256_file(&part_path)?;
        parts.push(PartInfo { name: part_name, size: this_chunk, sha256: sha });
        remaining -= this_chunk;
        idx += 1;
        tui::info(&format!("  part-{idx:03}/{} 完了", n_parts));
    }

    let manifest = SplitManifest { file: fname.clone(), size: total, sha256: full_sha, chunk_bytes, parts };
    let mpath = manifest_path_for(file);
    fs::write(&mpath, serde_json::to_string_pretty(&manifest)?)?;

    let tag = format!("model-{}", fname.to_lowercase().replace(".gguf", "").replace('.', "-"));
    println!();
    tui::succ("分割完了。GitHub Releases へのアップロード手順:");
    println!();
    println!("  1) gh CLI でログイン:      gh auth login");
    println!("  2) リリースを作成して全パートをアップロード:");
    println!();
    println!("     gh release create {tag} \\");
    let dir = file.parent().unwrap_or(Path::new("."));
    for p in &manifest.parts {
        println!("       \"{}\" \\", dir.join(&p.name).display());
    }
    println!("       --title \"{fname} mirror\" --notes \"loai model mirror\"");
    println!();
    println!(
        "  3) models.json の gh エントリ (tag: {tag}) が自動的に有効化されます。"
    );
    println!("  詳細: docs/models.md");
    Ok(manifest)
}

/// Join parts described by `manifest` into `dest`.
pub fn join_parts(manifest_path: &Path, dest: Option<&Path>) -> Result<PathBuf> {
    let text = fs::read_to_string(manifest_path).with_context(|| "マニフェストを読み込めません")?;
    let m: SplitManifest = serde_json::from_str(&text).context("マニフェストのJSONが不正")?;
    let dir = manifest_path.parent().unwrap_or(Path::new("."));
    let dest = dest
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| dir.join(&m.file));

    let mut out = File::create(&dest)?;
    for (i, p) in m.parts.iter().enumerate() {
        let ppath = dir.join(&p.name);
        if !ppath.exists() {
            // allow downloading from .part- files placed alongside
            return Err(anyhow!("パートが見つかりません: {} ({} 番目 / {})", ppath.display(), i + 1, m.parts.len()));
        }
        tui::info(&format!("検証+結合: {} [{}/{}]", p.name, i + 1, m.parts.len()));
        let sha = sha256_file(&ppath)?;
        if sha != p.sha256 {
            return Err(anyhow!("パート {} の sha256 不一致 — 再ダウンロードしてください", p.name));
        }
        let mut f = File::open(&ppath)?;
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }
    }
    out.flush()?;
    let meta = fs::metadata(&dest)?;
    if meta.len() != m.size {
        return Err(anyhow!("結合後サイズ不一致 (期待 {} / 実際 {})", m.size, meta.len()));
    }
    let sha = sha256_file(&dest)?;
    if sha != m.sha256 {
        return Err(anyhow!("結合後 sha256 不一致"));
    }
    tui::succ(&format!("結合完了: {} (検証OK)", dest.display()));
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_join_roundtrip() {
        let dir = std::env::temp_dir().join(format!("loai-split-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("model.gguf");
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&f, &data).unwrap();

        let m = split_file(&f, 16 * 1024).unwrap();
        assert_eq!(m.parts.len(), 7); // 100000 / 16384 = 6.1 → 7
        assert!(dir.join("model.gguf.part-000").exists());

        // remove original, join back
        fs::remove_file(&f).unwrap();
        let out = join_parts(&manifest_path_for(&f), None).unwrap();
        assert_eq!(fs::read(&out).unwrap(), data);

        for p in &m.parts {
            fs::remove_file(dir.join(&p.name)).ok();
        }
        fs::remove_file(manifest_path_for(&f)).ok();
        fs::remove_dir_all(&dir).ok();
    }
}
