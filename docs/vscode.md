# VS Code 連携ガイド

## 今すぐ使える: 統合ターミナル (v0.1)

loai はCLIツールのため、**VS Codeの統合ターミナルからそのまま利用できます**。

1. VS Codeでプロジェクトを開く → `` Ctrl+@ `` でターミナル表示
2. `loai agent "このプロジェクトのTODOを解消して"` を実行
3. エージェントがワークスペース内のファイルを読み・編集 (承認付き)・ビルド検証

### おすすめ設定

`.vscode/tasks.json` にloaiタスクを登録しておくと `` Ctrl+Shift+B `` から起動できます:

```json
{
  "version": "2.0.0",
  "tasks": [
    {
      "label": "loai agent (対話)",
      "type": "shell",
      "command": "loai",
      "args": ["agent", "${input:loaiTask}"],
      "presentation": { "echo": true, "reveal": "always", "focus": true, "panel": "shared" },
      "problemMatcher": []
    }
  ],
  "inputs": [
    {
      "id": "loaiTask",
      "type": "promptString",
      "description": "loaiに依頼するタスク"
    }
  ]
}
```

### キーボードショートカット化

`keybindings.json`:

```json
[
  {
    "key": "ctrl+alt+l",
    "command": "workbench.action.tasks.runTask",
    "args": "loai agent (対話)"
  }
]
```

## v2 ロードマップ: 専用拡張機能

- `loai.agent` コマンドパレット統合
- エディタ内インライン差分プレビュー (承認をエディタ上で y/n)
- 選択範囲をコンテキストにした質問 (`loai agent --selection "この関数のバグを直して"`)
- 問題パネル連携 (diagnostics をエージェントに還流)

拡張機能はloai CLIをラップする薄いTypeScript層として実装予定です (本体のRustロジックを再利用しないため挙動が一致します)。
