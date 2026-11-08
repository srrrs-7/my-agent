---
title: 大文字小文字を区別しない FS で、Unicode の別表記のファイル名が秘密ファイル保護と依存ファイル検知をすり抜けないか確かめる
created: 2026-11-08
urgency: low
importance: high
priority: P2
scope: [domain, infrastructure, docs]
---

# 大文字小文字を区別しない FS で、Unicode の別表記のファイル名が秘密ファイル保護と依存ファイル検知をすり抜けないか確かめる

## As-is（現状）

秘密ファイルの保護（`.env`、`*.key` などを読ませない・書かせない）と、依存ファイルの判定
（`Cargo.toml`、`package-lock.json` などへの書き込みに専用の承認を挟む）は、ファイル名を
**ASCII だけ小文字化して**比較しています。

macOS の APFS（大文字小文字を区別しない既定の構成）を Docker の bind mount で共有している場合、
次のような名前が**ホスト側では別の既存ファイルを開く可能性があります**（未検証）。

- `x.\u{212A}ey`（U+212A KELVIN SIGN）→ `x.key`
- `package-lock.j\u{017F}on`（U+017F LATIN SMALL LETTER LONG S）→ `package-lock.json`

もし開けるなら、字句の判定（パターン照合）にも正規化の判定（`guard()` の canonicalize。
大文字小文字を区別しない FS では入力の綴りが返りうる）にも引っかからず、モデルが秘密ファイルを
読んだり、依存の承認なしにマニフェストを書き換えたりできます。

## To-be（あるべき姿）

- 利用者の FS が大文字小文字を区別しなくても、どんな綴りで指定しても保護対象のファイルは
  保護され、依存ファイルへの書き込みは依存の承認を通る
- 再現しない環境・構成であれば、そのことが確認済みとして記録されている

## 影響範囲

- **クレート**: `domain/src/model/access.rs`（`Segment::glob` / `literal`、`AccessPolicy::permits`）、
  `domain/src/model/dependency.rs`（`ManifestKind::classify` / `classify_name`）、
  `infrastructure/src/tools/dependency_watch.rs`（L257、`run_command` 前後の比較対象の列挙）。
  `infrastructure/src/fs/local.rs` の `guard()` で実ファイル名を取り直す方向もありうる
- **不変条件**: **§2「ファイルアクセスは `WorkspacePath` 経由のみ」の字句段・正規化段の両方に
  触れる**。加えて、NFKC を domain で行うと `architecture.md` の domain の許可依存
  （std, serde, …）に無い crate（`unicode-normalization` 等）が要る — 依存を足すか、
  判定を infrastructure 側へ寄せるかは root の判断が要る
- **設定・ドキュメント**: `README.md` §5（秘密ファイル保護の表と「依存ファイルの変更」）
- **利用者への影響**: なし（拒否の範囲が広がるだけ。正当な非 ASCII のファイル名は従来どおり扱える
  ことを確認する必要はある）

## 達成条件

- [ ] **まず再現を試みる**: macOS ホスト + Docker Desktop の bind mount で、コンテナ内から
      `x.\u{212A}ey` / `package-lock.j\u{017F}on` を open したとき、ホストの `x.key` /
      `package-lock.json` が開くかを確認し、結果（Docker Desktop の版、共有方式 virtiofs 等を含む）を
      メモに記録する。macOS で直接 `agent` を動かした場合も同様に確認する
- [ ] 再現しない場合: その旨を記録し、P3 へ落とすか close するかを root に諮る
- [ ] 再現する場合: 上の 2 例と、ASCII 大文字（`X.KEY`）・合成済み／分解済みの別表記を含む
      ケースが、保護と依存判定の両方で弾かれることを domain の単体テストで固定する
- [ ] 再現する場合: 実ファイルシステム上で、別表記の名前経由で保護対象を読めないことを
      infrastructure のテストで固定する（大文字小文字を区別しない FS が CI で得られないなら、
      その制限を明記する）
- [ ] `make check` が緑

## 優先度の根拠

再現すれば秘密ファイル保護と依存ゲートの両方を迂回できる（重要度 `high`）。ただし未検証で、
macOS の大文字小文字を区別しない FS という特定の構成に限られ、後回しにしてもコストは
変わらない（緊急度 `low`）。score 7 → P2。再現した時点で緊急度を見直すこと。

## メモ

- 修正方向（レビュー案）: 照合の前に NFKC + Unicode case fold。std の `to_lowercase()` は
  case fold ではない（U+017F は既に小文字なので変わらない、U+212A は `k` になる）ため、
  std だけでは足りない。60 行で書ける範囲か、依存を足すかが論点（`invariants.md` §6）
- APFS は case fold に加えて正規化も区別しない（NFD/NFC の違いも同一視）。
  case fold だけ入れても正規化の差で漏れる可能性があるので、再現試験に含める
- 別解: `guard()` で `canonicalize` の結果ではなく、親ディレクトリを列挙して実際のエントリ名を
  取り直し、その名前で判定する（FS が同一視する名前を FS 自身に答えさせる）。
  依存が増えないが syscall が増える
- 関連: `done/2026-11-08_secret-file-protection.md`、`done/2026-11-08_dependency-change-gate.md`。
  MCP のツール名の「大文字小文字違いの衝突は未検出」（`done/2026-11-08_sandboxed-mcp-client.md`）
  は別件（ファイル名ではない）
- 出典: 2026-11-08 のレビュー（security）で見送ったもの
