//! HTTP downloader with resume (Range), progress and sha256 verification.
//! Primary source: Hugging Face. Fallback: GitHub Releases split parts.

use crate::models::{gh_asset_url, hf_url, GhSource, Profile};
use crate::tui;
use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn dl_agent(read_timeout: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(read_timeout))
        .user_agent(concat!("loai/", env!("CARGO_PKG_VERSION")))
        .build()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Download `url` to `dest` with resume support and optional verification.
pub fn download_with_resume(
    url: &str,
    dest: &Path,
    expected_size: u64,
    sha256: Option<&str>,
    label: &str,
) -> Result<()> {
    if dest.exists() {
        let ok = fs::metadata(dest).map(|m| m.len() == expected_size).unwrap_or(false);
        if ok {
            match sha256 {
                Some(want) => {
                    tui::info(&format!("{}: sha256 検証中...", label));
                    let got = sha256_file(dest)?;
                    if got == want {
                        tui::succ(&format!("{}: 既にダウンロード済み (検証OK)", label));
                        return Ok(());
                    }
                    tui::warn(&format!("{}: sha256 不一致のため再ダウンロードします", label));
                    fs::remove_file(dest).ok();
                }
                None => {
                    tui::succ(&format!("{}: 既にダウンロード済み", label));
                    return Ok(());
                }
            }
        } else {
            fs::remove_file(dest).ok();
        }
    }

    let part = dest.with_extension("part");
    let mut start: u64 = 0;
    if part.exists() {
        start = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if start >= expected_size {
            fs::remove_file(&part).ok();
            start = 0;
        }
    }

    if start > 0 {
        tui::info(&format!("{}: {} から再開します", label, tui::green(&crate::util::human_bytes(start))));
    }

    let agent = dl_agent(90);
    let req = agent.get(url);
    let req = if start > 0 {
        req.set("Range", &format!("bytes={}-", start))
    } else {
        req
    };
    let resp = req.call().map_err(|e| anyhow!("ダウンロード接続に失敗: {e}\nURL: {url}"))?;

    let status = resp.status();
    let mut resume = status == 206;
    if start > 0 && status == 200 {
        // server ignored Range — restart
        resume = false;
        start = 0;
        tui::warn("サーバーがレジューム非対応のため最初から取得します");
    }
    if status != 200 && status != 206 {
        return Err(anyhow!("HTTP {status} — ダウンロード失敗: {url}"));
    }

    let content_len = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let total = if resume { start + content_len } else { content_len };
    if total > 0 && total != expected_size && total != expected_size + start {
        // some CDNs add nothing; only warn on clear mismatch
        if total != expected_size {
            tui::warn(&format!(
                "サーバー報告サイズ ({}) とカタログ値 ({}) が異なります。続行します",
                crate::util::human_bytes(total),
                crate::util::human_bytes(expected_size)
            ));
        }
    }

    let mut file = if resume {
        OpenOptions::new().append(true).open(&part)?
    } else {
        File::create(&part)?
    };

    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 1024 * 256];
    let mut done = start;
    let mut prog = tui::Progress::new(label, expected_size.max(total));
    let mut hash = Sha256::new();
    if resume {
        // hash must cover whole file: hash existing part first
        let mut f = File::open(&part)?;
        let mut hb = vec![0u8; 4 * 1024 * 1024];
        loop {
            let n = f.read(&mut hb)?;
            if n == 0 {
                break;
            }
            hash.update(&hb[..n]);
        }
    }
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        hash.update(&buf[..n]);
        done += n as u64;
        prog.update(done);
    }
    drop(file);
    prog.finish(done);

    if done != expected_size {
        return Err(anyhow!(
            "サイズ不一致: 期待 {} / 取得 {} — ネットワークが中断された可能性があります。再実行すると続きから再開します",
            expected_size, done
        ));
    }

    if let Some(want) = sha256 {
        let got = hex::encode(hash.finalize());
        if got != want {
            fs::remove_file(&part).ok();
            return Err(anyhow!("sha256 不一致 (期待 {want}, 取得 {got}) — ファイルを破棄しました。再実行してください"));
        }
    }

    fs::rename(&part, dest).with_context(|| "一時ファイルの改名に失敗")?;
    Ok(())
}

/// Download a model: HF primary, GitHub split-parts mirror as fallback.
pub fn pull_model(profile: &Profile) -> Result<PathBuf> {
    let dest = profile.local_path();
    crate::config::ensure_dirs()?;

    let url = hf_url(&profile.hf.repo, &profile.hf.file);
    let hf_result = download_with_resume(&url, &dest, profile.hf.size, Some(&profile.hf.sha256), &profile.id);

    if hf_result.is_ok() {
        return Ok(dest);
    }

    if let Some(gh) = &profile.gh {
        tui::warn("Hugging Face からの取得に失敗しました。GitHub Releases ミラーを試行します...");
        let e = match hf_result {
            Err(e) => e,
            Ok(()) => unreachable!(),
        };
        tui::dim(&format!("理由: {e:#}"));
        return gh_download_parts(gh, &dest);
    }
    hf_result.map(|_| dest)
}

/// Download split parts from a GitHub release and join them into `dest`.
pub fn gh_download_parts(gh: &GhSource, dest: &Path) -> Result<PathBuf> {
    let tmp = crate::config::tmp_dir();
    fs::create_dir_all(&tmp)?;
    let mut part_paths: Vec<PathBuf> = Vec::new();

    for (i, asset) in gh.parts.iter().enumerate() {
        let part_path = tmp.join(format!("{}.dlpart-{:03}", gh.tag, i));
        let url = gh_asset_url(&gh.repo, &gh.tag, asset);
        // per-part expected size: last part is the remainder; we don't know it,
        // so we verify each part by final joined sha256 instead.
        let label = format!("{}/{}", gh.tag, asset);
        download_unknown_size(&url, &part_path, &label)?;
        part_paths.push(part_path);
    }

    // join
    tui::info("パーツを結合しています...");
    let joined = tmp.join(format!("{}.joined", gh.tag));
    {
        let mut out = File::create(&joined)?;
        for p in &part_paths {
            let mut f = File::open(p)?;
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
    }

    let meta = fs::metadata(&joined)?;
    if meta.len() != gh.size {
        return Err(anyhow!("結合後のサイズ不一致 (期待 {} / 実際 {})", gh.size, meta.len()));
    }
    tui::info("sha256 を検証しています...");
    let got = sha256_file(&joined)?;
    if got != gh.sha256 {
        return Err(anyhow!("sha256 不一致 (期待 {} / 実際 {})", gh.sha256, got));
    }
    fs::rename(&joined, dest)?;
    for p in part_paths {
        fs::remove_file(p).ok();
    }
    Ok(dest.to_path_buf())
}

/// Download a file of unknown size (no size check, no sha). Used for engine archives.
pub fn download_unknown_size(url: &str, dest: &Path, label: &str) -> Result<()> {
    let agent = dl_agent(90);
    let resp = agent.get(url).call().map_err(|e| anyhow!("{label}: 接続失敗: {e}"))?;
    if resp.status() != 200 {
        return Err(anyhow!("{label}: HTTP {} — ダウンロードに失敗しました", resp.status()));
    }
    let total = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut file = File::create(dest)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    let mut prog = tui::Progress::new(label, total);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        prog.update(done);
    }
    prog.finish(done);
    if total > 0 && done != total {
        return Err(anyhow!("{label}: サイズ不一致 ({total} 期待 / {done} 取得)"));
    }
    Ok(())
}
