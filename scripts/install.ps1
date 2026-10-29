# loai Windows インストーラ (PowerShell)
# 使い方: powershell -ExecutionPolicy Bypass -File scripts\install.ps1

$ErrorActionPreference = "Stop"

$InstallDir = Join-Path $env:USERPROFILE ".loai\bin"
$ApiBase = "https://api.github.com/repos/kagurasumusun/loai/releases/latest"

Write-Host "loai インストーラ" -ForegroundColor Cyan

# --- 1) loai 本体の取得 ---
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

function Get-LoaiBinary {
    Write-Host "最新リリースを確認中..." -ForegroundColor Gray
    try {
        $headers = @{ "User-Agent" = "loai-installer" }
        if ($env:GH_TOKEN) { $headers["Authorization"] = "Bearer $env:GH_TOKEN" }
        $rel = Invoke-RestMethod -Uri $ApiBase -Headers $headers -TimeoutSec 30
        $asset = $rel.assets | Where-Object { $_.name -eq "loai-x86_64-pc-windows-msvc.exe" } | Select-Object -First 1
        if ($asset) {
            $dest = Join-Path $InstallDir "loai.exe"
            Write-Host "ダウンロード: $($asset.name)" -ForegroundColor Gray
            Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $dest -UseBasicParsing
            Write-Host "loai.exe を $InstallDir に配置しました" -ForegroundColor Green
            return $true
        }
    } catch {
        Write-Host "リリース取得に失敗: $_" -ForegroundColor Yellow
    }
    return $false
}

$installed = Get-LoaiBinary
if (-not $installed) {
    Write-Host "ビルド済みバイナリが見つかりません。cargo でのインストールを試みます..." -ForegroundColor Yellow
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if ($cargo) {
        cargo install --git https://github.com/kagurasumusun/loai
    } else {
        Write-Host "cargo がありません。https://rustup.rs で Rust を導入するか、Releases から loai-x86_64-pc-windows-msvc.exe を手動ダウンロードして $InstallDir に置いてください。" -ForegroundColor Yellow
    }
}

# --- 2) PATH 登録 (ユーザースコープ) ---
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
    Write-Host "PATH に $InstallDir を追加しました (新しいターミナルで有効)" -ForegroundColor Green
}

# --- 3) 次ステップの案内 ---
Write-Host ""
Write-Host "次のステップ:" -ForegroundColor Cyan
Write-Host "  1. 新しいターミナルを開く"
Write-Host "  2. loai install      # llama.cpp (Vulkan + CPU) を導入"
Write-Host "  3. loai doctor       # ハードウェア診断"
Write-Host "  4. loai models pull qwen3-8b   # モデル取得 (約4.7GB)"
Write-Host "  5. loai agent `"タスク`"          # エージェント起動"
