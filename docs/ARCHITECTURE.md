> project-sapphire（`sapphire-journal` / `sapphire-agent` / `sapphire-ledger` / `sapphire-timer` と基盤 `sapphire-framework`）
> のローカルファースト基盤の設計ドキュメント。このリポジトリは旧 `sapphire-workspace` の履歴を
> 引き継いでおり、将来 `sapphire-workspace` リモートを `sapphire-framework` にリネームする前提。

## 背景と目的

各アプリは「**ファイルを原本・ローカルDBをキャッシュ**」というローカルファースト設計＋ git 同期＋MCP 連携を共有する。
現状の課題と、それに対する本設計の方針:

1. **git 同期はタイムラグがあり共同編集に不便** → Patroni による Postgres 分散を活かした中央集権型の
   *リモートワークスペース* を選択肢に追加する。ただし **サーバもクライアントと対称**（ファイル原本＋DBキャッシュ）にし、
   低レイテンシは git ではなく「サーバ仲介の change_log + 差分同期」で得る。（この中央サーバ方式はのちに p2p 同期へ置き換わった — 下記「ワークスペース同期」節）
2. **職場でネイティブアプリのインストールに懸念** → 環境を汚さない **WASM 版 journal**。
   ローカルキャッシュ(IndexedDB/OPFS)を持ち、リモートとのやり取りを「差分同期だけ」に絞る。
3. 共通機能は「workspace」の枠を超えるため、新リポジトリ **`sapphire-framework`**（全crate `sapphire-framework-*` プレフィクス）に集約する。（**WASM はのちに目標から外れた** — sync 仕様 決定 7）

## 確定した方針（ユーザー合意済み）

- **framework 移行を土台**にする。`sapphire-workspace` は **framework に吸収し廃止**。全crateは **`sapphire-framework-*`** プレフィクス
  （`sapphire-*` 一般名前空間を占有しないため意図的に長くする）。crates.io では新規crate名で publish。
  移行時のコード改変を最小化するため、依存宣言で Cargo の `package = "..."` エイリアスを使い、
  **コード内の extern 名（`sapphire_retrieve` 等）はそのまま維持**している。
- **キャッシュは純Rust製にして SQLite 依存を排除**（redb + tantivy。Cargo は feature が無効な
  optional 依存もバージョン解決して `links` の一意性を検査するため、SQLite 系は optional 化でなく
  削除 — 詳細はこの文書の「キャッシュバックエンド」節、記録は 2026-07-15 と 2026-08-28 の履歴）。
- **同期は中央サーバの JSON-RPC に一本化した #90 の方針は、p2p 同期で置き換えた**。
  framework 組込みの git 自動同期（`SyncBackend`/`ChangeSource` 抽象）の撤去自体はそのまま維持。
  git は**ユーザーが手動**で併用する。GUI 統合 git は将来ゼロベースで再構築（#91）。
- 同期は **iroh（QUIC）の上で 2 台の app server が直接セッションを張る p2p**（下の「ワークスペース
  同期」節）。sync 用の HTTP JSON-RPC と API キーは廃止した。API キー（`-keys`）が残るのは
  **非同期 HTTP エンドポイント**（agent の `/mcp` `/acp` `/a2a`）だけ。
- サーバも「ファイル原本＋DBキャッシュ」で対称（**Model B**）という対称性は p2p 設計の出発点として
  残る。全デバイスが同一構造を持ち、「常時稼働のピア」がサーバ役を担う（sync 仕様 §1）。
- **ベクター索引は同期対象外**。各ノードが各自保持する。セッションが運ぶのは**ファイル内容そのもの**
  （既定上限 64 MiB・hash でアドレス指定）。オフライン時の検索はローカル索引で行う。

## キャッシュバックエンド: SQLite 脱却（redb + tantivy）

**なぜ**: `sqlite-store` を必須にすると、sapphire-agent の matrix-sdk（rusqlite 0.37 / libsqlite3-sys 0.35 にピン）と
`libsqlite3-sys`（`links="sqlite3"`）が衝突しうる。調査の結果 **agent は元々 sqlite-store を使わず lancedb を使用**しており
衝突は未顕在だった。matrix に rusqlite バージョンを縛られないよう、framework のキャッシュから SQLite を無くす。

**sqlite-store は optional 化では不十分だったので削除済み**（2026-07-15）。
Cargo は **feature が無効な optional 依存もバージョン解決の対象にし**、`links` の一意性をそこで検査する。
そのため `sqlite-store` を切っていても framework の rusqlite はグラフに残り、ピン先が全消費者の制約になっていた:

- 0.37 にピン（matrix 合わせ）→ **journal が解決不能**（grain-id が rusqlite 0.39 / libsqlite3-sys 0.37 を要求）
- 0.39 に変更 → **agent が解決不能**（matrix-sdk-sqlite 経由）

両立する単一バージョンが存在しないため、optional のまま残すのではなく `sqlite_store.rs` ごと削除した。
これに伴い `VectorDb::SqliteVec` / `Error::SqliteStoreNotEnabled` / `open_sqlite_fts` / `open_sqlite_vec` /
`RetrieveDb::init_sqlite_vec` も廃止。`RETRIEVE_SCHEMA_VERSION` は常に 0（redb が自前で on-disk 形式を管理するため）。
**レガシー DB からの移行パスは無く、`db = "sqlite_vec"` の設定は `db = "redb"` に変更が必要。**

**LanceDB も削除済み**（2026-08-28）。redb をキャッシュの既定にした時点で、lancedb はベクトル索引
しか担わない二重化した経路になっていた。加えて lancedb は最新の 0.37.1 でも `arrow ^58` を要求するため、
arrow の更新がこちらの都合では進められない状態だった。`lancedb_store.rs` / `lancedb-store` feature /
`VectorDb::LanceDb` / `Error::LanceDbNotEnabled` を削除し、arrow・lance がグラフから消えたことで
`--all-features` のビルドも大幅に短くなった。**`db = "lancedb"` の設定は `db = "redb"` に変更が必要**
（enum から消えたので、そのままでは設定の読み込みが失敗する）。

**構成**（`sapphire-framework-retrieve`）:

- **`RetrieveStore` trait**（同期）が統一インターフェース（`upsert_document`/`remove_document`/`rebuild_fts`/
  `document_ids`/`document_count`/`embed_pending`/`vec_info`/`search_fts`/`search_similar`/`search_hybrid`）。
  mtime 追跡は責務外（`sapphire-framework-track` の `TrackStore` が持つ）。
- **唯一の永続実装 = `RedbStore`（redb + tantivy + brute-force vectors）**。C依存ゼロ・純Rust。
  `redb-store` を切ると揮発する in-memory ストアにフォールバックするだけなので、
  **各アプリは `redb-store` を既定に入れること**。
  - **redb** = 正本レコード保管。`documents: doc_id -> {path, chunks}`、`vectors: (doc_id,line_start) -> f32[]`、`meta`。
  - **tantivy** = redb から作る転置インデックス。**trigram トークナイザ**（`NgramTokenizer(3,3)`）で
    旧 FTS5 `trigram` 相当（substring・CJK 対応）。BM25 ランキング。索引は redb から再構築可能。
  - **ベクトル検索は brute-force**（redb 上の全ベクトルを L2 距離でスキャン）。数万件までミリ秒未満で厳密。
    規模が要求したら HNSW（`instant-distance` 等の純Rust）に差し替え可能。
  - **VectorStore を別 trait に切らず redb に統合**（vectors は同期対象外。非同期性は sync 層＝Change がドキュメントのみ運ぶことで担保）。
- **feature**: `redb-store`（既定）/ `fastembed-embed`。`sqlite-store` / `lancedb-store` は削除済み（上記参照）。
  `VectorDb` config enum は `None` / `Redb`（既定のブルートフォース）。

ストア分離の共有ヘルパー（`ChunkRow` / `group_by_file` / `vec_serialize` / `vec_deserialize` / `l2_distance`）は
`vector_store.rs` に集約し、sqlite / redb 両バックエンドで共用。

## crate 構成

Cargo workspace（モノレポ）。削除済みの crate も削除線で残す — 検索で引っかかったときの説明用。

| crate | 役割 |
|---|---|
| `sapphire-framework` | **単一依存ファサード**（bevy 方式・feature で各モジュール re-export）。既定 feature = `workspace` + `redb-store` |
| `sapphire-framework-workspace` | `AppContext` / `Workspace` / `WorkspaceState` / `AppKind` / ディレクトリ解決・移行（旧ルート lib。#90 で git/自動同期/device を撤去） |
| `sapphire-framework-track` | mtime 変更検知 `TrackStore`（redb） |
| `sapphire-framework-retrieve` | 検索。`RetrieveStore` + `RedbStore`（redb+tantivy）のみ |
| `sapphire-framework-sync` | 転送非依存のレプリケーションコア（wire 型・`ReplicaStore`(redb)・merge・HLC・コンフリクトコピー・フィルタ・外部編集検知） |
| `sapphire-framework-session` | 2 つのレプリカ間のセッション（フレーミング・vv 交換・差分と内容の転送） |
| `sapphire-framework-ipc` | ローカル IPC（UDS / 名前付きパイプ / プロセス内チャネル上の NDJSON JSON-RPC、ルータ、`connect` / `probe`） |
| `sapphire-framework-server` | アプリサーバ骨格（`workspace.*` 名前空間・多重管理・`FrameworkCommand` — `serve` / `status` / `service` / `workspace` / `workgroup` / `device` フラット語彙・同期ランタイム） |
| `sapphire-framework-bridge-api` | bridge 制御プレーンのプロトコルとクライアント（serde のみ・iroh 非依存）。**単独でバージョン管理**（2.0.0 — メジャー == 制御面 `API_VERSION`。`version.workspace` ではない） |
| `sapphire-framework-bridge` | ホスト常駐デーモン本体（デバイス同一性・workgroup 認可・ペアリング・交換台・iroh） |
| `apps/sapphire-bridge` | 上記のバイナリと CLI（`serve` / `status` / `log` / `service` / `workspace` / `workgroup` / `device`） |
| `sapphire-framework-registry` | デバイス台帳（`<dir>/<grain-id>.toml` を 1 デバイス 1 ファイル。`node_id` を保持。users は撤去） |
| `sapphire-framework-keys` | `KeyStore` / `AuthConfig` / `protect`。**非同期 HTTP エンドポイント**の認証用 |
| `sapphire-framework-service` | OS のサービスマネージャへの登録（`ServiceSpec`・systemd user unit・LaunchAgent・タスクスケジューラ） |
| `sapphire-framework-backend` | GUI 向け**非同期** `WorkspaceBackend` + `IpcBackend` / `LocalBackend`、`BackendEvent` |
| `sapphire-framework-gui` | app 非依存の egui `WorkspaceManager` / `WorkspaceRegistry` と同期 GUI 部品（下記「GUI 部品」） |
| ~~`sapphire-framework-rpc`~~ / ~~`-remote-client`~~ / ~~`-remote-server`~~ / ~~`-blob`~~ | **削除**（HTTP 同期スタック。表面テスト `tests/surface.rs` で存在を封じる。内容はファイル原本から直接供給される — sync 仕様 §2.3） |

**`-sync` と `-session` は `-workspace` / `-retrieve` に依存しない**（転送非依存の中核として。
`sapphire-sync` がこれを検証する）。**`-server` は iroh を引き込まない**（bridge 側の依存の軽い
`client` feature 経由で制御面だけを使う — プロセス構成仕様 §6）。

> **bridge の可視化**: `<bridge dir>/status.json`（5 秒ごと + 変化時、アトミック書き込み）と
> `<bridge dir>/logs/node.log`（10 MiB × 3 でローテーション）。書き手は単一インスタンスロックが
> 保証する 1 プロセスのみなので、ログは再起動をまたいで連続する。`sapphire-bridge status` は
> 稼働中の bridge に問い合わせ、応答が無ければ `status.json` を読む。

## GUI 部品（`sapphire-framework-gui`）

デスクトップの同期 GUI が共有する部品。2 層に分かれる。GUI は**サーバも bridge も自プロセス内で
起動しない** — 不在はエラーではなく「表示される状態」。

- **データ層（egui 非依存）**: `client::FrameworkClient` が app server と bridge に非同期で問い合わせ、
  1 回の更新で現況のスナップショット（サーバ・bridge・workgroup・デバイス・ワークスペース）を作る。
  コマンド（workgroup 作成・invite・参加・デバイス退避・ワークスペースの有効化/無効化/忘却）も
  同じクライアントが発行する。更新は 5 秒、コマンドは 30 秒（参加のみ 90 秒）で打ち切る
  （`ClientConfig::{fetch_timeout, command_timeout}` で変更可）— 相手が応答しなくても UI は固まらない。
  `views::*` はスナップショットから表示用の行・文言を作る**純関数**で、egui なしでテストできる。
- **表示層**: `SyncPanel` が上記を egui で描く（workgroup / デバイス / ワークスペースの管理）。
  invite チケットは画面にのみ出し、`tracing` には渡さない。`pub use egui` で利用側は egui の版を
  合わせられる。
- **フォント**: `fonts::system_cjk_font()` / `add_system_cjk_fallback(&mut FontDefinitions)` /
  `install_system_cjk_fallback(&egui::Context)` が OS 付属の CJK フォント（Windows / macOS / Linux）を
  フォールバックに足す。フォントファイルは同梱しない。

## GUI 向け 非同期 Backend trait

**framework 側は実装済み**（`sapphire-framework-backend`）: `#[async_trait]` の `WorkspaceBackend`
（`search`/`read_file`/`write_file`/`append_file`/`delete_file`/`list_dir`/`sync`/`subscribe`）+
`BackendEvent`（`tokio::sync::broadcast`）。実装は **`LocalBackend`**（同期 `WorkspaceState` を
`spawn_blocking` で包む）と **`IpcBackend`**（アプリサーバへの JSON-RPC。CLI・stdio MCP・desktop
が共有する経路）の 2 つ。native の Send フューチャ前提（egui は具象型保持で `runtime.spawn`）。
旧 `RemoteBackend`（中央サーバへ差分同期する GUI クライアント）は HTTP 同期スタックの削除とともに
廃止 — 同期そのものが p2p のサーバ側処理になったため、GUI は `IpcBackend` でサーバに依頼する。

**journal 側は後続 PR**（別リポジトリ・別仕様 — `2026-09-15-sapphire-sync-design.md` §5.3）:
現在 GUI が直接呼ぶ `ops::*` と `JournalState::*` を、GUI 依存の
`JournalBackend`（entries 粒度: `list_entries`/`get_entry`/`create_entry`/`update_entry`/`remove_entry`…）へ集約し、
`WorkspaceBackend` の上に載せる。

- **`LocalJournalBackend`**（native）= 既存同期 `JournalState`/`ops` を `spawn_blocking` で包む。純粋ロジックは残置。
- **サーバ経由** = `IpcBackend` 経由でアプリサーバに問い合わせる（sync の適用もサーバが行う）。
- egui は `dyn` を跨スレッド送信せず具象型を保持して `runtime.spawn`（既存 app.rs パターン）。

## ワークスペース同期（iroh・2 仕様）

同期は中央サーバの JSON-RPC ではない。2 台のレプリカが iroh (QUIC) の上で直接セッションを張り、
バージョンベクトルで差分を交換する。設計は 2 つの仕様に分かれている:

- `docs/superpowers/specs/2026-09-15-p2p-sync-iroh-design.md`（「sync 仕様」）— レプリケーション本体。
  §2（型・redb ストア・merge・HLC）と §6.1（そのテスト）は実装済みで**今も権威**。
  §3–§5 と §6.2–§6.3 は後述のプロセス構成仕様が置き換えたが、**破棄されず本文に残してある** —
  冒頭の置換表（ノード→bridge、follower 廃止、`SyncNode`→app server、`-net` の分割）に従って読むこと。
  決定 8 / 13（ホスト 1 ノードの lock 選挙、`sapphire-sync` を常駐 peer に）も superseded。
- `docs/superpowers/specs/2026-09-16-process-architecture-design.md` — プロセス構成。
  「誰が状態を所有するか」を 1 つの模型に統一し、実装順（sync 仕様 §5.6）は §9 が置き換えた。

### 所有者は 1 プロセス

redb のキャッシュは排他ロックで 1 プロセスしか開けない。だから全設計を貫くルールは
「**すべての状態には書き手がちょうど 1 つ**」:

| 状態 | 所有するプロセス |
|---|---|
| ワークスペースのファイル | そのアプリのサーバ |
| retrieve / track / アプリ固有のキャッシュ | 同じ |
| レプリカストア（`sync.redb`） | 同じ |
| デバイスの同一性（`node.key`）・workgroup 台帳・ペアリング | bridge |

つまり各アプリの **サーバ** がワークスペースを所有する。CLI・stdio MCP・desktop は
`sapphire-framework-ipc` 経由でサーバに接続する（UDS / named pipe / プロセス内チャネル上の
NDJSON JSON-RPC。**同一 OS ユーザー前提でトークンなし** — Unix はピア uid を検査して切断する）。
1 アプリ 1 サーバで複数ワークスペースを多重管理（LRU 閉じ）。サーバは SIGTERM/SIGINT
まで常駐し、OS サービス（systemd / LaunchAgent / タスクスケジューラ）として走る。CLI は
サーバを起動しない — サーバが無ければそれを報告して exit 1（start-on-demand と
`SpawnConfig` / idle-exit は廃止。起動するのは `serve` とサービスマネージャだけ）。
骨格は `AppServer`（`workspace.*` 名前空間は framework 所有、アプリは `<app>.<method>` を追加）。
CLI は全アプリ共通のフラット語彙 `serve` / `status` / `service` / `workspace` / `workgroup` /
`device` を `FrameworkCommand` として自分のサブコマンドの隣に flatten する（issue #142）。
制御面の路由は所有権に従う: `workspace` コマンドはアプリのサーバへ、`workgroup` / `device`
コマンドは bridge へ直接。`status` の報告書は CLI と IPC `server.info` が同じ
`StatusReport` 型を共有する（`backend::protocol` にあり、サーバ crate からも re-export）。
サーバは**ホストのワークスペース台帳**（`<config dir>/workspaces.toml`）を持ち、`workspace.list` が
それを返す（CLI の `workspace list` は稼働中のサーバに聞く）。`workspace.forget` は台帳から外して
同期も止める — ルートが既に無いワークスペースでも停止する。`serve` は起動時に台帳から同期中の
ワークスペースを復元する（再起動後に手動で有効化し直さなくてよい）。詳細はプロセス構成仕様 §2, §4。

### bridge はホスト常駐の交換台（`apps/sapphire-bridge`）

1 ユーザー 1 プロセスの独立バイナリ。`<データルート>/sapphire-bridge/`（`SAPPHIRE_BRIDGE_DIR`）に
`node.key`、`routes.toml`（workspace_id → 所有サーバ）、workgroup 台帳、`status.json`
（5 秒ごと＋変化時、アトミック）、`logs/node.log`（10 MiB × 3 ローテーション）を置く。
ワークスペースの中身は見ない。役割:

1. **同一性とペアリング** — `node.key` / NodeId。workgroup への参加は 1 回限りの invite
   チケット（`sapphire:…`、postcard + base32、既定 TTL 10 分）で `sapphire/pair/1` 上に行う。
2. **認可** — 接続相手の NodeId が共通 workgroup の非退避デバイスか。退避（retire）は台帳への
   同期変更として全デバイスに届く。これは bridge にしか決められない。
3. **交換台** — control（`bridge.register` 等の JSON-RPC: 登録・peer 一覧・status）と
   data（`{workspace_id, device_id}` ヘッダ + バイト列の生ストリームを所有サーバへ splice）。
   `net.wake_on_sync` が既定有効で、peer が来たとき停止中の所有サーバを起動する。ただし
   発火するのはレガシーな `ManagedBy::Spawned` 登録に対してのみで、start-on-demand 廃止後の
   新規登録（`Service`）では起動しない — Service 管理の所有者は自身の service manager が
   起動するもので、bridge も CLI もその manager ではない。よってオフライン報告になる。
   spawn 機構自体の再設計は Phase 2 の後続 issue で扱う。

workgroup のメタ（デバイス台帳・ワークスペース一覧）はそれ自体が同期されるワークスペースなので、
**bridge はそのアプリのサーバでもある**（アプリ名 `sapphire-bridge`、マーカー `.bridge/`）。
CLI は `sapphire-bridge`（`serve` / `status` / `service` / `workspace` / `workgroup` /
`device`）。この CLI もアプリ側と同じフラット語彙を話す — bridge 独自の `BridgeCommand`
（共有語彙を bridge 側で実行するもの）として、bridge 固有の `log` と並べて構成される。
`workgroup create` と `device retire` は、bridge 稼働中は制御面の `bridge.workgroup_create` /
`bridge.device_retire`（GUI も同じ口）を使い、停止中は bridge ディレクトリに直接書き込むため、
アプリ CLI 側の Phase 1 の同名ディレクティブも実在するコマンドを指すようになった。
`workspace` は読み取り専用の `list` のみ — ワークスペースを workgroup に置くのは
それを所有するアプリの仕事だからである（仕様 §1）。
詳細はプロセス構成仕様 §5。

### セッションはエンドツーエンド

`sapphire-framework-sync`（転送非依存のレプリケーションコア）と `sapphire-framework-session`
（セッション交換そのもの）が、bridge が splice したストリームの上で **2 台の app server 間で
直接** 走る。bridge は経路を作るだけでセッションには関与しない。

- `Hello`（形式・ワークスペース・replica id・vv）→ `PathUpdate` のページ → `Done`/`Settled`。
  形式不一致や別ワークスペースは `Refused`。64 KiB 以下の内容はインライン、それ以上は hash で
  `Want` → blob（受信側は SHA-256 を検証してから配置）。framing は tag + 4 バイト長。
- 外部編集検知は全ノードのフレームワーク動作（mtime+size のプリフィルタ → hash 比較）。
  行方不明のルート（未マウント等）ではレプリカを**停止**する（全ファイル消失と誤認して
  tombstone を撒くのを防ぐ — sync 仕様 §2.5）。
- 同時編集は DVV-set join で勝者を決め、敗者は
  `<stem>.conflict-<replicaのgrain-id>-<counter>.<ext>` という**コンフリクトコピー**として残す。
  内容が同一ならコピーは絶対に作らない。`.sapphireignore` が拒む名前や OS が表現できない名前は
  例外（sync 仕様 §2.4）。
- 同期するのはファイル内容のみ。mtime・権限は同期しない。ベクター索引は各ノードのローカル財産。
  サイズ上限は既定 64 MiB（チャンク分割・tombstone GC は後続 — sync 仕様 §2.8）。
- アプリ固有の修復（journal の重複 id 等）は content-deterministic・冪等・収束の契約のもとで
  アプリ自身が行う（sync 仕様 §5.1）。

ワークスペースを workgroup に置くのはアプリの CLI（`journal sync enable` /
`journal sync map <name|id> <dir>`）。同期の同一性はパス導出の uuid ではなく、マーカー内
`.<app>/sync-id` の grain-id（同期されるので全デバイスで一致）。

### 特権分離（撤去済み — issue #145）

特権分離は **撤去済み**（issue #145）。全ノード同一の権限管理は Windows 等で困難で、1ノードの
漏れが穴を再び開くため、機構ではなく運用（shell / fs ツールは管理者デバイス・管理者ルームのみ許可）で
対応する。`service install` は常にユーザーレベルの unit をインストールし、アプリはインストールした
ユーザーとして走る。framework の作るファイルとディレクトリはすべて `0700` / `0600`（これは維持）。
詳細はプロセス構成仕様 §3。

## アプリディレクトリ構成と CLI 規約（#128 / #129）

`dirs` は `sapphire-framework-workspace` の通常依存で、`clap` / `serde` とともにファサードから
re-export される（`sapphire_framework::{clap, serde, dirs}`）。ディレクトリ解決は framework 側の
`AppContext::init(AppKind)` が吸収する。

**レイアウト**: cache / data / config はどのカテゴリも `<プラットフォームルート>/<app-name>/`
直下（ワークスペースごとの内容は `<app>/<uuid>/`）。**per-kind の階層は無い**。
プラットフォームルート（`dirs::cache_dir()` 等）が解決できない場合は `std::env::temp_dir()` に
フォールバックする。`AppKind`（`cli` / `server` / `desktop`）はプロセスの種別として残るが、
**パスには現れない**。

> **`<app>/server/` をディスクで見つけたら**: #129 が一時的に導入した per-kind 分割
> （`<app>/<kind>/<uuid>/`）の残骸。desktop と server が 1 つの DB を取り合わないようにする
> ためのものだったが、プロセス構成の変更で「DB を開くのはサーバだけ」になり、さらに分割は
> 効いてくるべきケース（CLI と stdio MCP が同じ `cli` に解決して衝突する）を最初から覆って
> いなかったので、この一連の変更で撤去した。`init` が 2 回目の冪等移行（削除なし、server の
> コピー優先、競合は warn して残置）で `<app>/<kind>/…` を `<app>/…` へ戻す。空になった
> `<kind>/` ディレクトリは削除してよい。中身が残っていたら（= 複数 kind が同じワークスペースの
> キャッシュを書いていた）、各自で確認してから削除すること。

**環境変数名は統一規約**: カテゴリ別のオーバーライドは `SAPPHIRE_<APP>_<CATEGORY>_DIR`
（`CATEGORY` は `CACHE` / `DATA` / `CONFIG`）。置換するのは**プラットフォームルートのみ**。
ワークスペースルートは `SAPPHIRE_<APP>_DIR`。ワークスペース外の状態は per-app レイアウトの
外に置く — bridge ディレクトリは `<データルート>/sapphire-bridge/`（`SAPPHIRE_BRIDGE_DIR`）、
IPC ソケットは `<データルート>/sapphire-bridge/run/`（`SAPPHIRE_RUNTIME_DIR`）。ホスト = 1 デバイス
だから app 名も kind も持たない。

**一回限りの移行**（`init` 内、冪等、削除なし）:

- `keys.toml` は秘密情報なのでキャッシュツリーから data ツリーへ（UUID 単位の 1 回だけガード
  つき）。per-kind 時代のレイアウトを読むので、unsplit より先に走る。
- unsplit: `<app>/<kind>/…` → `<app>/…`（上記の blockquote 参照。競合時は server のコピーが勝ち）。

**CLI 引数の統一**: 共通引数 `WorkspaceArgs`（clap の `Args`。`#[command(flatten)]` で組み込む）
の正規名は `--workspace-dir`。旧名（`--journal-dir` / `--ledger-dir` / `--data-dir`）は
clap alias として受け付ける。解決順序は `Workspace::resolve` が担い、**明示引数 →
`SAPPHIRE_<APP>_DIR` → `SAPPHIRE_WORKSPACE_DIR`（warn 付き。deprecated のまま残る唯一の旧名）→
カレントディレクトリ** の順。

## 実装の現在地

実装順は `docs/superpowers/specs/2026-09-16-process-architecture-design.md` の §9 が権威
（sync 仕様 §5.6 の順序を置き換えた）。同節のステップは番号が振り直されているので、そちらの
番号で読むこと。現在地:

- **完了**: sync core（型・redb ストア・merge・HLC・コンフリクトコピー・フィルタ・外部編集検知 —
  sync 仕様 §2 / §6.1 が今も権威）、registry（users 撤去・1 デバイス 1 ファイル・`node_id`）、
  `-ipc`、アプリサーバ骨格（`workspace.*`・多重管理・`FrameworkCommand` — **元の問題 = CLI と
  stdio MCP のキャッシュ衝突がここで解決**）、bridge 基本部（ディレクトリ・単一インスタンス・制御面・データ面・iroh）、
  サーバの同期ランタイム（watcher・`Replica`・`sync.enable`）、ペアリングと workgroup、
  サーバ機能（組込み relay・`wake_on_sync`）、`-service`（特権分離は撤去済み — issue #145）。
- **進行中**: 後片付け — `-keys` の抽出、`-rpc` / `-remote-client` / `-remote-server` / `-blob`
  の削除、per-kind ディレクトリ分割の撤去、ファサード feature の組替え、**この文書の書き直し**。
- **後続**（§9 の外の計画レベルの項目）: 各アプリの移行（journal / ledger / timer / agent —
  各自のリポジトリと仕様）、`sapphire-sync` を「1 アプリ」として作ること、crates.io への
  publish（アプリが git 依存をやめるまで不可）。
- **対象外**: **WASM**。journal 等のブラウザ版は目標から外れた（sync 仕様 決定 7。
  #86 steps D–F も対象外）。indexeddb/OPFS キャッシュも組みません。

## 既知のリスク / 難所

1. 同期→非同期の波及は Backend trait のみ async 化で封じる（`ops::update_entry(&Connection,...)` の `&Connection` を trait から外す破壊的変更）。
2. egui native の async: `?Send` により `dyn` は跨スレッド不可 → 具象型保持 + `runtime.spawn`。
3. 非互換 crate（git2・fastembed・sqlx-postgres・tantivy/redb・iroh）は native 専用バイナリで隔離。
   プラットフォーム差（UDS / named pipe / チャネル、systemd / LaunchAgent / タスクスケジューラ）
   は各層が吸収する。
4. tantivy trigram FTS の挙動同等性（BM25・prefix フィルタ・短いクエリ<3文字は無マッチ＝FTS5同等）。
5. サーバを格上げした代償（プロセス構成仕様 §12）: サーバが無いときの 1 回きりの CLI コマンドは
   「サーバが走っていない」報告で終わり、起動待ちは発生しない（start-on-demand 廃止に伴う
   見直し。元の「起動待ちで遅くなる」は受け入れて廃止）。**ディレクトリの 2 度目の一括移行**
   （#129 の直後に unsplit）。NFS ホームでは UDS が使えない。混雑したホストでは bridge + アプリごとのサーバが並ぶ。
6. storage backend の将来差替（Postgres+S3）。`OriginStore` trait を切る。content-addressed hash の GC は後続。
