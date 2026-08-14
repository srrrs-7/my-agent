---
title: ネットワーク名前空間による完全隔離層（SandboxKind::NetnsProxied）
created: 2026-08-14
urgency: low
importance: medium
priority: P3
scope: [infrastructure, docs]
---

# ネットワーク名前空間による完全隔離層（SandboxKind::NetnsProxied）

## As-is（現状）

`run_command` は Landlock で封じ込められていますが、操作者が「このコマンドは
許可ドメイン以外に絶対到達しない」と言い切れる状態にはなっていません。
2 つの穴が残っており、どちらも Landlock では**原理的に塞げません**。

1. **同一ポートの別ホストに到達できる。** Landlock のネットワークルールはポート単位で、
   カーネルの `landlock_net_port_attr` に宛先ホストの欄がありません。したがって
   「プロキシのポートだけ許可」は「そのポート番号なら誰にでも繋げる」と同義です。
   `HTTP_PROXY` に従う正直なプログラムは allowlist に従いますが、悪意ある
   build script は raw socket で迂回できます。
2. **ワークスペース内の `.git` を守れない。** Landlock のルールはカーネルが *union* する
   純粋な許可リストで、拒否も「より具体的なルールが勝つ」規則もありません。
   書き込み可能なワークスペースの中の一部だけを読み取り専用にすることが表現できず、
   コマンドは履歴の書き換えや hook の設置ができます。

現状は `crates/infrastructure/src/exec/linux.rs` のコメント、README §5、および
`landlock_cannot_protect_git_inside_the_writable_workspace` テストで
**制限として明記**されています。「気づいていない」のではなく「この層では表現できない」
という状態です。

## To-be（あるべき姿）

操作者が `AGENT_SHELL_SANDBOX=isolated` を選べば、上記 2 つが閉じます。

- コマンドから見えるネットワークは egress プロキシだけになり、ポート番号を合わせても
  他のホストには到達しない
- `.git` を含む「書き込み可能ツリー内の読み取り専用サブツリー」を表現できる
- 現在は `isolated` を指定すると必ず起動失敗するが、Linux では成功するようになる

## 影響範囲

- **クレート**: `crates/infrastructure/src/exec/`（`linux.rs` と並ぶ新モジュール）
- **不変条件**: `invariants.md` §2 の子プロセス節（「黙って弱くならない」を維持すること）
- **設定・ドキュメント**: `.env.example` の `AGENT_SHELL_SANDBOX`、README §5 の表、
  `management/docs/architecture.md` §3.2.1
- **利用者への影響**: 破壊的変更なし。既定は `confined` のまま

## 達成条件

- [ ] `AGENT_SHELL_SANDBOX=isolated` が Linux で成功し、`SandboxKind::NetnsProxied` を報告する
- [ ] 子プロセスがプロキシのポート番号で**別ホスト**に接続できないことをテストで固定する
      （Landlock 層では通ってしまう経路が、この層では塞がれていること）
- [ ] 子プロセスが `.git` に書き込めないことをテストで固定する
      （コマンド実行 issue から移送した条件。拒否を表現できる機構が要るため）
- [ ] **外部バイナリのインストールを必要としない**。これが満たせないなら本 issue は
      「実装しない」で閉じる方が誠実
- [ ] 使えない環境では `confined` へ**黙って落ちない**（要求されたら起動失敗）
- [ ] `make check` が緑

## 優先度の根拠

Landlock 層で「ワークスペース外への書き込み」「正直なプログラムの egress」は既に
塞がっており、残るのは悪意あるコードを想定した迂回路です（重要度 medium）。
既定の承認ポリシーでコマンドは毎回人間が承認するため、悪意あるコマンドはまず
そこで止まります。回避策として「使い捨てコンテナで動かす」も有効なので緊急度 low。

## メモ

- 素直な実装は `unshare(CLONE_NEWNET|CLONE_NEWNS|CLONE_NEWUSER)` + veth なしの
  loopback のみの netns + プロキシを netns 内から見える形で配置、`.git` は
  read-only bind mount。ただし **`unshare(CLONE_NEWUSER)` は Docker の既定 seccomp
  プロファイルで CAP_SYS_ADMIN なしには拒否される**ことをこの dev container で実測済み
  （`CapEff: 0000000000000000`、`unprivileged_userns_clone=1` でも不可）。
  つまり「コンテナの中で動く CLI」では使えない可能性が高く、達成条件の
  「外部バイナリ不要」と同じくらい「特権不要」が争点になる。
- claude code は bubblewrap（要 `apt install`）、codex も同様の外部依存を持ち、
  どちらもコンテナ内での機能低下をドキュメントで認めている。同じ道を選ぶなら
  「外部依存ゼロ」という本プロジェクトの差別化を捨てることになるため、
  **この層は追加の tier であって既定にはしない**。
- 関連: `management/issues/done/2026-08-14_sandboxed-command-execution-tool.md`、
  `management/issues/todo/2026-08-14_macos-seatbelt-command-sandbox.md`
