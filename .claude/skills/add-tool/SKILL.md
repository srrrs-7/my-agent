---
name: add-tool
description: my-agent に LLM が呼び出せる新しいツールを追加する。application 層の実装から composition root への登録、テスト、ドキュメント更新までの手順。「〜できるツールを追加して」と言われたときに使う。
argument-hint: [tool-name]
---

# 新しいツールを追加する

$ARGUMENTS

ツールは**ユースケース**なので `application` 層に置きます。外界に触れる必要があるなら
`domain` のポート経由です。`infrastructure` を直接呼んではいけません。

参考実装: `crates/application/src/tools/file/read.rs`（読み取り系）、`edit.rs`（変更系）。

## 1. 設計を決める

| 決めること | 判断基準 |
|---|---|
| ツール名 | `[a-zA-Z0-9_-]{1,64}`。動詞_目的語（`read_file`, `search_files`） |
| `ToolSafety` | `ReadOnly` は**状態を一切変えない**場合のみ。並列実行の対象になる。変更するなら `Mutating`、不可逆なら `Destructive` |
| 必要なポート | 既存（`FileSystem` / `FileSearcher`）で足りるか。足りないなら `domain/src/ports/` に追加 |

新しいポートが必要なら、先に `domain-specialist-agent` に配置を確認してください。

## 2. 実装する

`crates/application/src/tools/<category>/<name>.rs`:

```rust
pub struct XxxTool { /* Arc<dyn Port>, Arc<WorkspaceRoot> */ }

#[derive(Debug, Deserialize)]
struct Input { /* #[serde(default)] を厚めに */ }

impl XxxTool {
    pub fn new(...) -> Self { ... }
    fn name() -> ToolName { ToolName::new("xxx").expect("static tool name is valid") }
}

#[async_trait]
impl Tool for XxxTool {
    fn definition(&self) -> ToolDefinition { /* name, description, input_schema, safety */ }
    async fn execute(&self, arguments: Value) -> Result<ToolOutcome, ToolError> { ... }
}
```

守ること:

- **パスは `self.root.resolve(&input.path)` で解決する。** 生の文字列から組み立てない
- 引数は `parse_arguments(&name, arguments)` で読む（モデルが JSON 文字列で包んで送る事故を吸収する）
- `description` の**1 行目**がシステムプロンプトに載る。ここで何をするツールかを言い切る。
  2 行目以降には使い方の注意（例: 「編集前に必ず read すること」）
- `input_schema` は JSON Schema。`required` と `additionalProperties: false` を書く
- 出力は `ToolOutcome::new(content).with_summary(...)`。summary は端末の 1 行表示用
- **エラーメッセージはモデル向け。** 何が失敗したかより、次に何をすべきかを書く

## 3. 登録する

1. `crates/application/src/tools/<category>/mod.rs` で `pub use`
2. `crates/cli/src/composition.rs` の `ToolRegistry` に追加する。
   **`TimeoutTool::wrap(..., timeout)` で包むのを忘れない**（1 回の暴走がループを止めないため）

## 4. テストする

- ツール単体: 正常系、引数不正、対象なし、境界値
- サンドボックス: ワークスペース外のパスが拒否されること
- 必要なら `crates/cli/tests/end_to_end.rs` に、モデルがこのツールを呼ぶシナリオを追加

`.claude/rules/testing.md` の方針に従ってください。

## 5. ドキュメントを更新する

- `README.md` §4 のツール表
- ツール数が変わるので `make tools` の出力と README の記述が一致するか確認

## 6. 確認する

```bash
make check
make tools    # 定義がモデルにどう見えるかを目視する
```

`make tools` の出力を必ず読んでください。description の 1 行目が意図どおりか、
引数の必須／任意が正しく出ているかは、ここでしか気づけません。
