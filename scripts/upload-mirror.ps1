# loai モデルミラー公開スクリプト (GitHub Releases分割アップロード)
# 前提: gh CLI で `gh auth login` 済みであること
# 使い方: powershell -ExecutionPolicy Bypass -File scripts\upload-mirror.ps1 -ModelFile "Qwen3-8B-Q4_K_M.gguf"

param(
    [Parameter(Mandatory = $true)]
    [string]$ModelFile,
    [int]$ChunkMB = 1900
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $ModelFile)) {
    Write-Error "ファイルが見つかりません: $ModelFile"
    exit 1
}
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    Write-Error "gh CLI が必要です: https://cli.github.com/"
    exit 1
}

Write-Host "loai ミラーアップローダ" -ForegroundColor Cyan
Write-Host "対象: $ModelFile (チャンク ${ChunkMB}MB)" -ForegroundColor Gray

# 1) loai split で分割 + マニフェスト生成
loai split $ModelFile --chunk-mb $ChunkMB
if ($LASTEXITCODE -ne 0) { Write-Error "分割に失敗"; exit 1 }

# 2) マニフェストからタグ名を組み立て
$name = [System.IO.Path]::GetFileName($ModelFile)
$tag = "model-" + $name.ToLower().Replace(".gguf", "").Replace(".", "-")
$parts = Get-ChildItem -Path . -Filter "$($name).part-*" | Sort-Object Name

Write-Host "リリースタグ: $tag / パート数: $($parts.Count)" -ForegroundColor Gray

# 3) GitHub Release に全パートをアップロード
$args = @("release", "create", $tag) + ($parts | ForEach-Object { $_.FullName }) + @("--title", "$name mirror", "--notes", "loai model mirror (auto-split)")
& gh @args
if ($LASTEXITCODE -ne 0) { Write-Error "アップロードに失敗"; exit 1 }

Write-Host ""
Write-Host "完了。models.json の gh エントリ (tag: $tag) が有効化されました。" -ForegroundColor Green
Write-Host "ダウンロード側は HF 失敗時に自動でこのミラーを使用します。" -ForegroundColor Gray
