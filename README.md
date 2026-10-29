# loai

**極限最適化ローカルコーディングエージェント** — 16GB RAM + Intel iGPU (Iris Xe) の Windows 11 PCで、重量級OSSコーディングモデルを快適に動かすためのCLIツールキットです。

既存のローカルAIツールが「重い・メモリが足りない・満足に動かない」16GBクラスのPC向けに、**モデル選定 → 量子化 → llama.cpp起動パラメータ → エージェントの文脈管理** まで全段階を個別に極限チューニングしました。

```
$ loai agent "このリポジトリのTODOコメントをすべて解消して"
● list_files ...
● read_file src/parser.rs
◆ 承認が必要: edit_file src/parser.rs
  -     // TODO: implement
  +     fn parse(input: &str) -> Result<Ast> { ... }
  [y]実行 [n]拒否 [a]以後許可 [q]停止 > y
● run_command cargo check
■ 完了
```

## なぜ16GBで軽く動くのか

| 最適化ポイント | 通常のツール | loai |
|---|---|---|
| KVキャッシュ | fp16 (ctx 32kで数GB) | **q8_0 / q4_0 量子化** (1/2〜1/4) |
| Flash Attention | 未指定が多い | `-fa on` 自動有効 |
| 文脈管理 | ファイルを丸ごと読ませ爆発 | **トークン予算制御 + リポジトリマップ + 古いツール出力の自動省略** |
| GPU活用 | CPUのみ or 全オフロード失敗 | **Vulkan (Iris Xe) → CPU 自動フォールバック + 実測ベンチ** |
| 実行形態 | Electronアプリ/重いランタイム | **Rust単一バイナリ、常駐メモリ数MB** |
| モデル選定 | 13Bを詰めて破綻 | RAM実測に基づく自動提案 (`loai doctor`) |
| 起動失敗 | エラーで終了 | **5段の自動縮退ラダー** (思考kwargs解除→FA解除→ngl=0→CPU切替) |

## クイックスタート (Windows 11)

```powershell
# 1) loai 本体 (Releases から loai-x86_64-pc-windows-msvc.exe をダウンロード、または)
cargo install --git https://github.com/kagurasumusun/loai

# 2) llama.cpp エンジン (Vulkan + CPU、公式夜間ビルドを自動DL)
loai install

# 3) ハードウェア診断 → 推奨モデルを確認
loai doctor

# 4) モデル取得 (Hugging Face、中断しても再開可能)
loai models pull qwen3-8b

# 5) 起動
loai agent "READMEに英語版を追加して"
```

## モデルカタログ (各プロファイルを個別極限チューニング)

| ID | モデル | ファイル | 必要RAM | 使用見込* | 特徴 |
|---|---|---|---|---|---|
| `qwen3-8b` | Qwen3-8B Q4_K_M | 4.7GB | 10GB | 約6.1GB | **主力**。思考モード対応の最新世代 |
| `qwen2.5-coder-7b` | Qwen2.5-Coder-7B Q4_K_M | 4.4GB | 8GB | 約5.1GB | コーディング特化 |
| `qwen3-4b-2507` | Qwen3-4B-Instruct-2507 Q4_K_M | 2.3GB | 8GB | 約3.8GB | 高速 (2507更新版) |
| `qwen3-14b-tight` ⚠実験 | Qwen3-14B **IQ3_XXS** + KV q4_0 | 5.5GB | 10GB | 約6.5GB | 16GBに載る最大級Dense |
| `gpt-oss-20b` ⚠実験 | gpt-oss-20b MXFP4 (MoE/活性3.6B) | 11.3GB | **20GB** | 約12.1GB | 32GB向け。16GBでは他アプリ完全終了が前提 |
| `qwen2.5-coder-3b` | Qwen2.5-Coder-3B Q4_K_M | 1.8GB | 6GB | 約2.3GB | 超軽量 |
| `qwen2.5-coder-1.5b` | Qwen2.5-Coder-1.5B Q4_K_M | 0.9GB | 4GB | 約1.4GB | 最小構成・動作確認用 |

\* weights + 量子化KV (16k ctx) + バッファの実測ベース見積り。`loai doctor` が貴方のPCで正確に計算します。

GLM-4.5 / GLM-4.6 / Qwen3-Coder-30B 等の大規模モデルが16GBで動かない理由と必要RAM早見表は **[docs/models.md](docs/models.md)** を参照してください。

## コマンド一覧

```
loai install              llama.cpp導入 (Vulkan + CPU)
loai doctor               ハードウェア診断 + モデル互換表 + 推奨
loai bench [--apply]      生成速度実測 (Vulkan vs CPU比較、--applyで最速を保存)
loai models list          カタログ表示
loai models pull <ID>     モデルDL (HF優先、失敗時GitHubミラー、レジューム+sha256検証)
loai models verify <ID>   sha256完全検証
loai models use <ID>      既定モデル設定
loai chat                 ツールなしチャット (/think で思考表示)
loai agent "タスク"        自律コーディングエージェント
loai split <file>         GitHub Releases用に2GB分割 (ミラー公開用)
loai stop                 残存llama-serverの停止
```

### loai agent のオプション

```
--model <ID>       モデル指定 (省略時はdoctor推奨)
--max-turns N      最大ターン数 (既定24)
--think            思考モード有効化 (遅いが高品質)
--reasoning        思考プロセスをリアルタイム表示
--yes              全ツール自動承認 (CI向け・危険)
--api-base URL     外部OpenAI互換API (LM Studio / Ollama等) を直接利用
```

エージェントは `list_files` / `read_file` / `search_files` / `edit_file` / `write_file` / `run_command` / `finish` の7ツールを持ち、**ファイル変更とコマンド実行には必ず差分プレビュー付きの承認**を要求します。ワークスペース外へのパスはサンドボックスで遮断され、破壊的コマンド (rm -rf / 等) は即時拒否されます。

## 対応環境

- **Windows 11** (x64) — メインターゲット。Intel iGPU (Iris Xe/UHD/Arc) は Vulkan バックエンドで自動オフロード
- **Linux** (x64) — 開発・CI用
- llama.cpp 公式夜間ビルド (b-tagリリース) を自動取得。最新版が取れない場合はピン留めビルド (b11977) にフォールバック

## ビルド

```bash
cargo build --release     # Rust 1.82+
cargo test                # ユニットテスト
```

依存クレートはわずか10個 (anyhow / clap / serde / serde_json / ureq / crossterm / regex / sha2 / hex / zip)。すべて純Rustで、単一バイナリを出力します。

## モデルの配布について

- **一次配布**: Hugging Face (bartowski氏・ggml-org公式の量子化版。ライセンス的に正規)
- **フォールバック**: このリポジトリの GitHub Releases に分割ミラーを置けます (後述)
- **ミラーの作り方**: `loai split model.gguf` で 1.9GB分割 + マニフェスト生成 → `gh release create` でアップロード → models.json の `gh` エントリが自動的に有効化。ダウンロード側は HF 失敗時に自動で分割パーツを取得・結合・sha256検証します。詳細は [docs/models.md](docs/models.md)。

## ロードマップ

- [ ] VS Code 拡張 (統合ターミナル経由でv0.1から利用可能 → [docs/vscode.md](docs/vscode.md))
- [ ] SYCLバックエンド自動ベンチ (Intel oneAPI公式ビルドの自動導入)
- [ ] imatrix量子化の自前生成 (`loai quantize`)
- [ ] 会話履歴の永続化と再開
- [ ] 複数タスクの並列エージェント

## ライセンス

MIT License — llama.cpp (MIT) / Qwen (Apache-2.0) / gpt-oss (Apache-2.0) と整合します。

### 謝辞
- [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp) — 推論エンジン
- [bartowski](https://huggingface.co/bartowski) — 高品質量子化GGUF
- [Qwen](https://huggingface.co/Qwen) / [OpenAI](https://huggingface.co/openai-community) — オープンウェイトモデル
