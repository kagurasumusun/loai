//! Hardware diagnosis: RAM/CPU/GPU detection and model recommendation.

use crate::models::Catalog;
use crate::tui;
use anyhow::Result;
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct Hardware {
    pub os: String,
    pub cpu: String,
    pub cores: usize,
    pub logical: usize,
    pub ram_total: u64,
    pub ram_available: u64,
    pub gpus: Vec<String>,
}

impl Hardware {
    /// RAM budget available for the model+KV+overhead.
    pub fn model_budget(&self) -> u64 {
        let os_reserve: u64 = if cfg!(windows) { 4 * 1024 * 1024 * 1024 } else { 1 * 1024 * 1024 * 1024 };
        let by_free = (self.ram_available as f64 * 0.95) as u64;
        let by_total = self.ram_total.saturating_sub(os_reserve);
        by_free.min(by_total).max(512 * 1024 * 1024)
    }
}

/// Budget used by Catalog::resolve (fast path, may spawn powershell on Windows).
pub fn model_budget_bytes() -> u64 {
    detect().map(|h| h.model_budget()).unwrap_or(8 * 1024 * 1024 * 1024)
}

pub fn detect() -> Result<Hardware> {
    let mut hw = Hardware {
        os: std::env::consts::OS.to_string(),
        ..Default::default()
    };
    hw.logical = std::thread::available_parallelism().map(|v| v.get()).unwrap_or(4);
    hw.cores = crate::util::physical_cores().max(1);

    #[cfg(target_os = "windows")]
    {
        let ps = r#"
$m = [PSCustomObject]@{
  ram = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory)
  free = [math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory * 1KB)
  cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name
  gpus = @((Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name }))
}
$m | ConvertTo-Json -Compress
"#;
        if let Ok(out) = Command::new("powershell").args(["-NoProfile", "-Command", ps]).output() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&out.stdout)) {
                hw.ram_total = v["ram"].as_u64().unwrap_or(0);
                hw.ram_available = v["free"].as_u64().unwrap_or(0);
                if let Some(c) = v["cpu"].as_str() {
                    hw.cpu = c.trim().to_string();
                }
                if let Some(g) = v["gpus"].as_array() {
                    hw.gpus = g.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            let mut total = 0u64;
            let mut avail = 0u64;
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("MemTotal:") {
                    total = v.trim().split_whitespace().next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0) * 1024;
                }
                if let Some(v) = line.strip_prefix("MemAvailable:") {
                    avail = v.trim().split_whitespace().next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0) * 1024;
                }
            }
            hw.ram_total = total;
            hw.ram_available = avail;
        }
        if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
            if let Some(l) = text.lines().find(|l| l.starts_with("model name")) {
                if let Some(n) = l.split(':').nth(1) {
                    hw.cpu = n.trim().to_string();
                }
            }
        }
        if let Ok(out) = Command::new("sh").arg("-c").arg("lspci 2>/dev/null | grep -iE 'vga|3d|display' || true").output() {
            let t = String::from_utf8_lossy(&out.stdout);
            for line in t.lines().filter(|l| !l.trim().is_empty()) {
                if let Some(idx) = line.find(": ") {
                    hw.gpus.push(line[idx + 2..].split(" (").next().unwrap_or("").trim().to_string());
                }
            }
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        hw.ram_total = 8 * 1024 * 1024 * 1024;
        hw.ram_available = 4 * 1024 * 1024 * 1024;
        hw.cpu = "unknown".into();
    }

    if hw.ram_total == 0 {
        // fallback assumption
        hw.ram_total = 8 * 1024 * 1024 * 1024;
        hw.ram_available = 4 * 1024 * 1024 * 1024;
    }
    Ok(hw)
}

pub fn recommend<'a>(cat: &'a Catalog, hw: &Hardware) -> &'a crate::models::Profile {
    let budget = hw.model_budget();
    cat.profiles
        .iter()
        .filter(|p| !p.experimental && p.estimated_memory_bytes(p.tuning.ctx) <= budget)
        .max_by_key(|p| p.weights_bytes)
        .unwrap_or_else(|| {
            cat.profiles
                .iter()
                .filter(|p| !p.experimental)
                .min_by_key(|p| p.weights_bytes)
                .unwrap_or(&cat.profiles[0])
        })
}

pub fn run() -> Result<()> {
    let hw = detect()?;
    let cat = Catalog::load()?;
    let budget = hw.model_budget();
    let rec = recommend(&cat, &hw);

    println!("{}", tui::cyan("══ loai doctor — ハードウェア診断 ══"));
    println!();
    println!("  OS            : {}", hw.os);
    println!("  CPU           : {} (物理 {} コア / 論理 {})", hw.cpu, hw.cores, hw.logical);
    println!("  RAM           : 合計 {} / 空き {}", crate::util::human_bytes(hw.ram_total), crate::util::human_bytes(hw.ram_available));
    println!("  GPU           : {}", if hw.gpus.is_empty() { "検出されず (CPUモード)".to_string() } else { hw.gpus.join(", ") });
    println!("  モデル予算    : 約 {} (OS確保分・ヘッドマージンを除く)", crate::util::human_bytes(budget));
    println!();
    println!("{}", tui::cyan("── モデルプロファイル互換性 ──"));
    println!();
    println!("  {:<22} {:>9} {:>9}  {:<6} {}", "ID", "必要RAM", "使用見込", "状態", "備考");
    for p in &cat.profiles {
        let need = p.estimated_memory_bytes(p.tuning.ctx);
        let mark = if need <= budget {
            if p.experimental { tui::yellow("△実験") } else { tui::green("○可能") }
        } else {
            tui::red("×過大")
        };
        let note = if p.experimental { "実験的 (要調整)" } else { "" };
        println!(
            "  {:<22} {:>9} {:>9}  {:<6} {}",
            p.id,
            format!("{}GB", p.min_ram_gb),
            crate::util::human_bytes(need),
            mark,
            note
        );
    }
    println!();
    let igpu = hw.gpus.iter().any(|g| {
        let g = g.to_lowercase();
        g.contains("iris") || g.contains("uhd") || g.contains("arc") || g.contains("radeon") && !g.contains("rx")
    });
    println!("  推奨プロファイル: {} {}", tui::green(&rec.id), tui::dim(&format!("({})", rec.name)));
    if igpu {
        println!("  推奨バックエンド: {} (Intel iGPU は Vulkan でオフロード可能 / `loai bench` でCPU比較を推奨)", tui::green("vulkan"));
    } else {
        println!("  推奨バックエンド: {} (GPU非検出のため)", tui::green("cpu"));
    }
    println!();
    println!("{}", tui::dim("次のステップ: loai install → loai models pull <ID> → loai agent \"タスク\""));
    Ok(())
}
