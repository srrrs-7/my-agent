---
title: 会話の新鮮度で圧縮を重み付けする（メッセージに順序を持たせる）
created: 2026-08-14
started: 2026-08-14
completed: 2026-08-14
urgency: medium
importance: medium
priority: P2
scope: [domain, application, infrastructure, cli, docs]
---

# 会話の新鮮度で圧縮を重み付けする（メッセージに順序を持たせる）

## As-is（現状）

履歴に残すものを決めているのは**通数**だけだった。`AGENT_COMPACT_KEEP_RECENT=12` は
「直近 12 通」であり、その 12 通が一行の返答でも 32 KB のツール結果でも同じ扱いになる。

通数は尺度になっていない。12 通は一行の返答なら 2 KB、ツール結果なら 384 KB で、
後者は `AGENT_MAX_HISTORY_BYTES`（既定 256 KB）を単独で超える。守るはずだった直近の
やり取りが、書いたばかりの要約を予算から押し出して削除させる。

畳む側も境目でばっさりだった。境界の 1 つ手前と 1 つ後ろで扱いが全く違い、
「新しいものほど残る」が段階になっていない。

## To-be（あるべき姿）

同じ大きさの会話なら、**新しいものほど長く原文のまま残る**。何通残るかは、その通が
実際どれだけ大きかったかで決まる。ツール結果 1 通が直近の会話を丸ごと押し出すことがない。

畳む側も段階的で、直近の続きに使われやすい手前のターンは詳細のまま要約器に渡り、
底の方は輪郭だけが渡る。どれだけ古くても、記述が消えてなくなることはない。

## 影響範囲

- **クレート**:
  - `domain` … `Message::seq`（セッション内の通番）、`Conversation` が通番を採番、
    `compaction_split_within`、`distance_from_newest`
  - `application` … `agent/compaction.rs`（容量上限の適用と距離に応じた切り詰め）、
    `agent/config.rs`
  - `infrastructure` … `config/mod.rs`（新しい環境変数）
  - `cli` … `composition.rs` への結線
- **不変条件**: **触れるのは §4 のみ**（当初 §1・§5 にも触れる想定だったが、
  時計をやめたことで不要になった。下の「決めたこと」を参照）
  - **§4 すべての `tool_call` に結果を返す** — 容量で決めた切り位置も必ず
    `Conversation::fold_boundary` を通す。テストで固定済み
- **設定・ドキュメント**: `.env.example`、`README.md` §4 / §7 / §8、
  `management/docs/architecture.md` §3.2.2
- **利用者への影響**: プロバイダへ送る JSON は変わらない（`openai.rs` / `anthropic.rs` は
  `WireMessage` に手で写しているので `Message` の新フィールドは送信されない）。
  `AGENT_COMPACT_KEEP_RECENT` の意味は「上限」に変わったが、既存の設定値はそのまま有効

## 達成条件

- [x] 各メッセージに順序が記録され、セッションを通じて一貫している
      （`sequence_numbers_survive_a_fold_and_keep_counting`）
- [x] 判断が壁時計に依存しない。ドメインにクロックのポートを足さずに実現する
- [x] 同じ通数でも、大きい会話ほど早く畳まれることをテストで固定する
      （`the_byte_cap_shortens_the_verbatim_tail`、
      `a_bulky_tail_is_folded_further_than_the_message_count_would`）
- [x] 後ろに下がるほど要約器に渡る情報が薄くなり、かつ消えないことを固定する
      （`a_blocks_share_of_the_transcript_halves_with_distance`、
      `the_transcript_thins_out_towards_the_bottom`）
- [x] 圧縮で生成した**要約が薄められない**ことを固定する
      （`a_distant_summary_is_still_carried_forward_whole`）
- [x] 容量で決めた切り位置が孤児の `tool_result` を作らず、履歴が `user` で始まる
      （`a_byte_driven_cut_still_never_orphans_a_tool_result`）
- [x] 容量を実質無制限にすると通数だけの現行と同一の判断になる
      （`the_message_cap_still_applies_when_the_bytes_are_generous`）。
      既存の圧縮・刈り込みテストは無改変で緑
- [x] 順序を含む `Conversation` が serde でラウンドトリップでき、順序のない旧データも読める
      （`the_sequence_number_is_optional_in_both_directions`、
      `a_history_without_sequence_numbers_still_measures_distance`）
- [x] 新しい設定値が `.env.example` と README §4 / §8 に載っている
- [x] `make check` が緑（288 passed / 0 failed）

## 優先度の根拠

「新しいものほど残る」は**位置**では既に成立しており、この issue が効くのは位置と大きさが
ずれる場面に限られる。回避策（`AGENT_COMPACT_KEEP_RECENT` を下げる）もあるので重要度は
`medium`。セッション永続化が先に入ると保存済みセッションのスキーマ移行が必要になり、
後回しにするほど手戻りが増えるため緊急度も `medium`（score 6 → P2）。

## 決めたこと

### 新鮮度は「経過時間」ではなく「メッセージの順序と大きさ」で測る

起票時は壁時計で測る想定で、`Clock` ポートの追加・`SystemClock` 実装・composition への結線まで
書き始めていた。着手中に方針を変更し、**時計を一切使わない**構成にした。

効くべきなのは「それ以降どれだけ会話が進んだか」であって、何時間経ったかではない。
3 時間放置しただけのセッションは何も先に進んでいないのに、時計はそれを「古い」と報告する。

副作用として、当初「あり」だった不変条件への影響が 2 つ消えた。

| 不変条件 | 時計版 | 順序版 |
|---|---|---|
| §1 依存は内向きのみ | `Clock` ポートの追加が必要 | **不要**（順序はセッションが自分で数える） |
| §5 テストは実モデルを要求しない | 偽クロックの注入が前提 | **構造的に決定的**（注入するものがない） |
| §4 孤児の `tool_result` を作らない | `fold_boundary` を通す | 同左（変わらず必要） |

`crates/application/src/agent/session.rs` の「this crate needs neither a clock nor a random
source」というコメントも、書き換えずに済んだ。

### 「直近 N 通」に容量の上限を足した（通数は残した）

`compaction_split_within(keep_recent, keep_recent_bytes)` は新しい方から遡って、
**通数と容量の両方が許す間だけ**取る。既定は 64 KB（`AGENT_COMPACT_KEEP_RECENT_BYTES`）。

通数を捨てて容量だけにしなかったのは、既存の `AGENT_COMPACT_KEEP_RECENT` を無効にしないため。
容量を大きくすれば従来と同一の判断に戻る（テストで固定）。

**最新の 1 通だけは容量に関係なく必ず残す。** 答えるべきターンそのものであり、
畳んでしまうとモデルに答える対象がなくなる。

### 畳む側は半減方式にした

1 ブロックが要約器に渡せるバイト数は、8 メッセージ後ろに下がるごとに半分になり、
250 バイトで下げ止まる（`block_budget`）。正規化した傾斜ではなく固定の減衰にしたのは、
畳む深さによって同じ距離のメッセージの扱いが変わらないようにするため。

下限をゼロにしないのは、「ここに何か手順があったが説明できない」より
短い説明の方がましだから。

### ユーザー発言の無条件保護はそのまま残した

距離に関係なく切り詰めない。前回の要約は user ロールで履歴の先頭に入るので、この保護が
そのまま効く。ここを外すと**圧縮のたびに前回の圧縮を打ち消す**ことになり、
最も情報密度の高い 1 通が真っ先に削られる。`a_distant_summary_is_still_carried_forward_whole`
で固定した。

## 結果

- **domain** — `Message::seq`（`Option<u64>`、serde は `default` + `skip_serializing_if`）、
  `Conversation` が `push` / `from_messages` で採番、`replace_prefix` は要約に
  「畳んだ最後のメッセージの通番」を継がせる（最初のではない。要約を最古扱いにすると、
  削除を止めるために書いた 1 通が最初に削られる）。
  `compaction_split_within` と `distance_from_newest` を追加
- **application** — `CompactionConfig.keep_recent_bytes`、`render_transcript` が
  `Conversation` と split を受け取って距離ごとに `block_budget` を適用
- **infrastructure / cli** — `AGENT_COMPACT_KEEP_RECENT_BYTES`（既定 65536）を結線
- **ドキュメント** — `.env.example`、README §4（新しい節）/ §7（テスト数）/ §8（設定表）、
  `management/docs/architecture.md` §3.2.2
- **テスト** — 11 本追加、277 → 288。`make check` 緑
- **やらなかったこと**
  - `trim_to_budget`（刈り込み）には重み付けを効かせていない。あちらは要約を作らずに
    削除する最後の砦で、重み付けは削除を増やす方向にしか働かない
  - セッション再開時の重み付けは、セッション永続化が入っていないので対象外
    （`todo/2026-08-14_session-persistence-and-resume.md`）。`seq` はそのための
    順序キーとして使える形にしてある
  - 既定値（64 KB）は実測ではなく設計上の見積もり。`AGENT_MAX_HISTORY_BYTES` の 1/4 で、
    要約（約 8 KB）を足しても次の圧縮まで十分な余地が残る。実 LLM での検証は
    `todo/2026-08-14_verify-against-a-real-llm.md` に含めるのがよい
