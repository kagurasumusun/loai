# モデルカタログと大規模モデル互換表

## 16GB RAM の物理的な限界について

Windows 11 はアイドルで約4〜5GBを消費します。16GB PCでモデルに割けるのは実質 **約10GB** です。これは「最適化の工夫」でどうにかなる領域ではなく、メモリに載せる重さ (パラメータ数 × 量子化ビット幅) の物理計算です。

## 正直な互換表 (よくある大規模モデル)

| モデル | 総パラメータ | Q4量子化時の重さ | 16GB PC | 必要マシン |
|---|---|---|---|---|
| **GLM-4.5 / GLM-4.6** | 355B (MoE) | **約90〜100GB** | ❌ 不可能 | 128GB RAM / H100クラス複数枚 |
| **GLM-4.5-Air** | 106B (MoE) | 約30GB (Q4相当で更に圧縮可) | ❌ | 64GB RAM |
| **GLM-Z1-32B** | 32B | 約19GB | ❌ | 32GB RAM |
| **GLM-4-9B** | 9B | 約5.5GB | ⚠ チャット可 (loai未収録: ツール呼び出しテンプレートのllama.cpp対応が弱い) | 16GB |
| **Qwen3-Coder-30B-A3B** | 30B (MoE, 活性3B) | 約18.6GB (Q4_K_M) | ❌ | 32GB RAM |
| **DeepSeek-V3 / R1** | 671B (MoE) | 約350GB超 | ❌ | サーバー |
| **gpt-oss-120b** | 117B (MoE, 活性5.1B) | 約63GB | ❌ | 80GB RAM |
| **gpt-oss-20b** | 21B (MoE, 活性3.6B) | 約11.3GB (MXFP4) | ⚠ 実験収録 | 32GB推奨 |
| **Qwen3-14B** | 14.8B | 9.0GB (Q4_K_M) / **5.5GB (IQ3_XXS)** | ⚠ 実験収録 (IQ3_XXS) | 16GB (緊縮) |
| **Qwen3-8B** | 8.2B | 5.0GB (Q4_K_M) | ✅ **主力** | 16GB |

**MoEモデルの注意**: 活性パラメータが小さくても「全エキスパートの重みがRAMに載る」ため、16GBでは総パラメータがそのまま壁になります。gpt-oss-20b は MXFP4 ネイティブ4bit でも 11.3GB 必要です。

## loai 収録プロファイルの詳細チューニング根拠

### qwen3-8b (主力)
- **Q4_K_M** (5.03GB): IQ4_XSより僅かに高品質、Q5_K_M (5.85GB) との差は誤差級。16GBでの総使用量6.1GB (KV q8_0@16k含む) が最適点
- **KV q8_0**: fp16 (2.25GB@16k) → 1.21GB。品質劣化は体感不可
- **`-fa on`**: KV量子化の前提条件。Vulkan/CPU両方で動作
- **エージェント時 `--chat-template-kwargs {"enable_thinking": false}`**: 5〜15 tok/s の低速環境では思考トークンがコスト大。`--think` で反転
- 温度: 非思考0.7 / 思考0.6 (Qwen推奨値)

### qwen3-14b-tight (実験: 大規模Dense)
- **IQ3_XXS** (5.94GB): imatrix適用済み3bit。Q3_K_M (7.3GB) より1.4GB削減
- **KV q4_0**: @16kで0.67GB (fp16なら2.6GB)。3bit KVは劣化ありだが3bit重みと合わせて総量6.5GBに収める設計
- 品質はQ4の8Bと同等か僅かに上。長文コード生成で優位

### gpt-oss-20b (実験: 大規模MoE)
- **MXFP4** (12.1GB): OpenAIネイティブ4bit。追加量子化は不可 (これが最小構成)
- **KV fp16**: gpt-ossはハイブリッドアテンション (スライディングウィンドウ) のためKVが小さく、量子化不要
- **`-fa` 無効**: gpt-ossのFA組み合わせは環境差が大きいため安全側に
- **ctx 8192**: RAM最優先。16GB PCではブラウザ等の完全終了が前提。失敗時は自動的にKV縮退

### qwen2.5-coder-7b / 3b / 1.5b
- コーディング特化系。KV q8_0、`--jinja` によるHermes形式ツール呼び出し
- 7BはGQA 4 KVヘッドのみでKVが極小 (57KB/token) → ctx 16kでも0.5GB

## モデル配布の仕組み

### 一次: Hugging Face 自動ダウンロード
```
loai models pull qwen3-8b
```
- `hf.co/{repo}/resolve/main/{file}` から直接DL
- **Range再開**: 中断しても続きから (`.part` ファイル保持)
- **sha256検証**: models.json 埋め込み値 (HF LFS oid) と照合。不一致時は破棄

### フォールバック: GitHub Releases 分割ミラー
GitHub のアセット制限は **1ファイル2GiB**。loaiはこれを複数アセットに分割して回避します。

**ミラー公開側の手順** (リポジトリ所有者):
```powershell
# 1. 分割 (1.9GB毎 + マニフェスト生成)
loai split Qwen3-8B-Q4_K_M.gguf

# 2. 出力された gh コマンドを実行
gh release create model-qwen3-8b `
  "Qwen3-8B-Q4_K_M.gguf.part-000" `
  "Qwen3-8B-Q4_K_M.gguf.part-001" `
  "Qwen3-8B-Q4_K_M.gguf.part-002" `
  --title "Qwen3-8B Q4_K_M mirror" --notes "loai model mirror"
```
models.json の `gh` エントリ (`tag: model-<id>`) と照合し、HF失敗時に自動で分割パーツをDL→結合→sha256検証します。

**注意**: GitHubの利用規約上、Releasesを大容量配布に使う行為は推奨されないため、あくまでフォールバック用途です。一次はHugging Faceを利用してください。

## カスタムモデルの追加

`~/.loai/models/` にGGUFを置き、`models.json` にエントリを追加すれば任意モデルを登録できます (バイナリと同じ場所の models.json が優先されます)。`kv_bytes_per_token` は `レイヤー数 × KVヘッド数 × ヘッド次元 × 2(K+V) × 2バイト(fp16)` で計算してください。
