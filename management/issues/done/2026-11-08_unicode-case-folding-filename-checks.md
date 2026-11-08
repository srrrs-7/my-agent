---
title: 大文字小文字を区別しない FS で、Unicode の別表記のファイル名が秘密ファイル保護と依存ファイル検知をすり抜けないか確かめる
created: 2026-11-08
urgency: low
importance: high
priority: P2
started: 2026-11-08
completed: 2026-11-08
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

- [x] **まず再現を試みる**: macOS ホスト + Docker Desktop の bind mount で、コンテナ内から
      `x.\u{212A}ey` / `package-lock.j\u{017F}on` を open したとき、ホストの `x.key` /
      `package-lock.json` が開くかを確認し、結果（Docker Desktop の版、共有方式 virtiofs 等を含む）を
      メモに記録する。macOS で直接 `agent` を動かした場合も同様に確認する
- [ ] ~~再現しない場合: その旨を記録し、P3 へ落とすか close するかを root に諮る~~（該当なし: 再現した）
- [x] 再現する場合: 上の 2 例と、ASCII 大文字（`X.KEY`）・合成済み／分解済みの別表記を含む
      ケースが、保護と依存判定の両方で弾かれることを domain の単体テストで固定する
- [x] 再現する場合: 実ファイルシステム上で、別表記の名前経由で保護対象を読めないことを
      infrastructure のテストで固定する（大文字小文字を区別しない FS が CI で得られないなら、
      その制限を明記する）
- [x] `make check` が緑

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

## 結果（2026-11-08）

### 再現試験 — 再現した

環境: macOS（APFS、大文字小文字を区別しない既定構成）、Docker Server 29.2.1（lima の VM、
Ubuntu 24.04 / kernel 6.8.0-147）、`/workspace` は **virtiofs** の bind mount。

- **コンテナ内（bind mount 上）**: `x.key` と `package-lock.json` を作り、別表記で `cat` した。
  `X.KEY` / `x.\u{212A}ey` / `package-lock.j\u{17F}on` / `PACKAGE-LOCK.JSON` はすべて実ファイルの
  中身を返した。`package-lock.j\u{17F}on` への書き込みは**既存の `package-lock.json` を上書き**し、
  新しいエントリは増えなかった。`realpath x.\u{212A}ey` は**入力の綴りのまま**を返した
  （= `guard()` の canonicalize では実名に戻らない）
- **macOS ホスト直接**（Python の `open()`）: 上記に加え、ケースフォールドかつ NFD で ASCII になる
  全 13 文字（Unicode 13 のデータから列挙: `ß` `ſ` `;`(U+037E) `ẞ` `` ` ``(U+1FEF) `K`(U+212A)
  `ﬀ` `ﬁ` `ﬂ` `ﬃ` `ﬄ` `ﬅ` `ﬆ`）が**すべて**対応する ASCII 名を開いた。`Å`(U+212B) / `Ω`(U+2126)、
  NFC/NFD の差（`résumé`）も同一視された。全角 `ｋ`、`ı`(U+0131)、`İ`(U+0130) は別名。
  ゼロ幅文字など 15 種の無視可能文字はいずれも名前を区別した（APFS は無視しない）。
  → APFS は「完全ケースフォールド + NFD」で比較し、NFKC（互換分解）はしない
- `agent` バイナリを macOS で直接動かす確認はしていない。対策は FS に問い合わせる前の字句判定なので、
  実行場所に依存しない

### 採った対策

`domain/src/model/file_name.rs` の `fold()` を新設し、`AccessPolicy`（パターン・例外・照合対象）と
`ManifestKind::classify` / `classify_name`（`.cargo` / `config` の判定を含む）の比較を
ASCII 小文字化からこれに置き換えた。依存は増やしていない。

- ASCII はそのまま小文字化、**ケースフォールド + NFD が ASCII になる非 ASCII 文字の表（上の 13 文字）**
  は ASCII に読み替え、NTFS の大文字比較向けに `ı` → `i` も足した。それ以外は `char::to_lowercase`
- ケースフォールドも NFD も文字単位で働く（NFD の並べ替えは結合文字＝非 ASCII しか動かさない）ので、
  名前全体が ASCII に畳まれるのは各文字が ASCII に畳まれるときだけ。よってこの表で
  **ASCII で書かれた規則（組み込み規則のすべて）に対しては漏れがない**
- 「`guard()` で親ディレクトリを列挙して実名を取り直す」案は採らなかった。既存ファイルには効くが、
  **まだ無いファイルを別表記で作る場合**（`package-lock.j\u{17F}on` を新規作成 → 以後 npm が
  `package-lock.json` として開く）に効かず、依存ゲートは結局字句判定が要る。また経路の各段で
  readdir が増える
- 「非 ASCII を含み ASCII に畳むと一致する名前を拒否」案と効果は同じだが、照合キーとして畳む方が
  保護・依存判定・例外（`.env.example`）を同じ 1 関数で扱える
- `infrastructure/src/tools/dependency_watch.rs:257`（`SKIPPED_DIRS` の照合）は**変えていない**。
  走査から外す側の一覧なので、畳み込みを広げると監視が弱くなる方向にしか働かない

### テスト

- domain: `file_name` 3 本、`access` 2 本（別表記の拒否、非 ASCII で書いた運用者規則）、
  `dependency` 1 本（別表記のマニフェスト・`.cargo/con\u{FB01}g.toml` を含む）
- infrastructure: `fs::local` の
  `a_unicode_spelling_of_a_protected_name_is_refused_on_a_real_filesystem`。一時ディレクトリを
  crate のソース横（開発コンテナでは bind mount = APFS）に作り、まず FS が `x.\u{212A}ey` で
  `x.key` を開くことを確かめ（開かない FS ではその部分だけ skip して理由を出す）、そのうえで
  `LocalFileSystem` が読み書きとも拒否することを assert する。開発コンテナでは skip されずに通った。
  Linux の CI（ext4 の `/tmp` ではなくチェックアウト先に作るが、大文字小文字を区別する FS）では
  別名の実証部分は skip になる

### 残した既知の問題

- **非 ASCII で書いた運用者規則**（`AGENT_DENY_READ` / `AGENT_DENY_WRITE`、ワークスペース内に
  置いたセッションログのディレクトリ名）は大文字小文字だけを同一視し、合成済み／分解済みの差は
  同一視しない（Unicode の分解表が要るため）。README §5 に「両方の綴りを列挙する」と書いた
- 表は Unicode 13 のデータから作った。将来の版で ASCII に畳まれる文字が追加されれば追従が要る
  （ケースフォールドの安定性方針上、既存文字の畳み込み先は変わらない）
- 優先度根拠に「再現した時点で緊急度を見直すこと」とある。対策を入れたので見直しは root に委ねる

