# アーキテクチャ

```
┌─────────────────────────────────────────────┐
│ loai (Rust単一バイナリ)                       │
│                                             │
│  main.rs ── clap CLI                         │
│    ├── doctor.rs   ハードウェア検出・推奨      │
│    ├── models.rs   カタログ (models.json埋込) │
│    ├── download.rs HF/GHダウンローダ (再開付) │
│    ├── engine.rs   llama.cpp管理・起動ラダー   │
│    ├── llm.rs      OpenAI互換SSEクライアント  │
│    ├── chat.rs     チャットREPL               │
│    ├── split.rs    2GB分割/結合               │
│    └── agent/                               │
│        ├── mod.rs      自律ループ             │
│        ├── tools.rs    7ツール+サンドボックス  │
│        ├── context.rs  文脈予算・リポマップ    │
│        └── approve.rs  承認UI・差分表示        │
└───────────────┬─────────────────────────────┘
                │ HTTP (127.0.0.1:動的ポート /v1)
┌───────────────▼─────────────────────────────┐
│ llama-server (llama.cpp公式夜間ビルド)        │
│  Vulkan (Iris Xe) / CPU 自動切替             │
│  -fa on / KV q8_0 / --jinja                  │
└───────────────┬─────────────────────────────┘
                │ mmap
┌───────────────▼─────────────────────────────┐
│ ~/.loai/models/*.gguf (Q4_K_M量子化)          │
└─────────────────────────────────────────────┘
```

## ディレクトリ構成

```
loai/
├── Cargo.toml
├── models.json          # モデルカタログ (sha256/サイズ/チューニング)
├── src/
│   ├── main.rs          # CLIディスパッチ (clap derive)
│   ├── config.rs        # ~/.loai/config.json
│   ├── util.rs          # トークン推定/glob/ウォーク
│   ├── tui.rs           # ANSIカラー/プログレスバー
│   ├── models.rs        # Profile/カタログ
│   ├── download.rs      # レジューム+sha256検証DL
│   ├── engine.rs        # llama.cpp導入/起動/ベンチ/停止
│   ├── doctor.rs        # RAM/CPU/GPU検出 (PowerShell//proc)
│   ├── llm.rs           # SSEストリーム+ツールコール蓄積
│   ├── chat.rs          # チャットREPL
│   ├── split.rs         # GH用2GB分割
│   └── agent/           # 自律エージェント
├── docs/
├── scripts/             # インストーラ/ミラー公開スクリプト
└── .github/workflows/   # CI (build+test) / リリースビルド
```

## 主要データフロー

### モデル取得
`loai models pull <id>` → models.json から `hf` エントリ取得 → `hf.co/{repo}/resolve/main/{file}` をRange再開付きDL → `.part` 完成後にsha256照合 → `~/.loai/models/` へ改名。HF失敗時のみ `gh` エントリ (分割パーツ) にフォールバック。

### サーバー起動
`loai agent` → `install_engine` (未導入ならllama.cpp公式zip/tarをDL・展開) → 空きポート自動割当 → チューニング引数でllama-server起動 → `/health` が `"ok"` を返すまでポーリング (モデルサイズに応じた動的タイムアウト) → 失敗なら縮退ラダーで再試行。

### エージェントループ
システムプロンプト (行動規範 + リポジトリマップ) + タスク → `/v1/chat/completions` (stream) → ツールコール蓄積 → 各ツール実行 (変更系は承認ゲート) → `role:"tool"` で結果還流 → 文脈トリミング → 次ターン。`finish` またはツールなし応答で終了。

## 設計判断の要点

- **外部LLM SDKに依存しない**: 自前SSEパーサ (~200行) でllama-server/LM Studio/Ollama v1 と直接通信。ビルド軽量化とバージョン安定のため
- **blocking I/O + スレッド**: tokio等の非同期ランタイム非搭載。CLI用途では起動が速くメモリフットプリントが最小
- **llama.cppをサブプロセス管理**: Rustに埋め込まずプロセス分離。llama.cppのライセンス/ビルドから独立し、異常時のログも分かる
- **models.jsonを埋め込み**: バイナリ単体でカタログ保持。バイナリ隣接のmodels.jsonで上書き可能 (フォーク向け)
- **決定論的文脈トリミング**: LLM要約を挟まないため、挙動が予測可能でトークン消費も安定

## テスト

`cargo test` に15のユニットテストを同梱:
- トークン推定器・glob・ヒューマンサイズ
- カタログ整合性 (sha256長・サイズ一致・メモリ見積り範囲)
- SSEフォールバックツールコールパーサ (Hermes形式/空形式/誤検知なし)
- 文脈トリミング (予算内収束・システム/最初のタスク保護)
- edit_file 一意性検査・サンドボックス脱出拒否 (`..`・絶対パス)
- split/join ラウンドトリップ (sha256検証付き)
