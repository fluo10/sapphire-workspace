# sapphire-framework（日本語）
> 言語: [English](README.md) | **日本語**

ファイルベースのワークスペースのためのローカルファーストフレームワーク：インデックス、検索、同期、（予定の）リモート／WASMバックエンド。 かつて `sapphire-workspace` として公開されており、再利用可能なクレートは現在 `sapphire-framework-*` プレフィックス付きでここに置かれています。

`sapphire-framework-workspace` クレートは `sapphire-framework-retrieve`（全文＋ベクトル検索）を、ファイルベースのドキュメントに対する単一で使いやすいAPIにまとめます。同時編集は中央のリモートサーバー（`sapphire-framework-remote-*`）が処理し、ローカルでの自動同期は行いません。gitは手動で使います。

## 機能

| 機能フラグ | 有効にするもの | デフォルト |
|---|---|---|
| `redb-store` | redbレコード＋tantivy全文インデックス＋総当たりベクトル | 有効 |

## クイックスタート

```toml
[dependencies]
sapphire-workspace = "0.10"
```

### ワークスペースの初期化

```rust
use sapphire_workspace::{AppContext, Workspace};

let ctx = AppContext::new("my-app");

// cwdから上へ辿り、`.my-app`マーカーディレクトリを見つけるまで探索。
let ws = Workspace::find_with_ctx(ctx, std::env::current_dir()?)?;
println!("root: {}", ws.root.display());
println!("uuid: {}", ws.uuid);
println!("cache: {}", ws.ctx.cache_dir().display());
```

### 開いてインデックス

```rust
use sapphire_workspace::{AppContext, Workspace, WorkspaceState};

let ctx = AppContext::new("my-app");
let ws = Workspace::find_with_ctx(ctx.clone(), std::env::current_dir()?)?;
let state = WorkspaceState::open(ws, ctx)?;

// すべてのMarkdown / JSON / JSONLファイルをretrieve DBへ増分的に同期。
let (upserted, removed) = state.sync()?;
println!("{upserted} upserted, {removed} removed");
```

### 全文検索

```rust
let results = state.retrieve_db().search("my query", 10)?;
for r in results {
    println!("{}: {}", r.path, r.score);
}
```

### ファイルの読み出し

```rust
use std::path::Path;

// ファイル全体を読む
let content = state.read_file(Path::new("notes/hello.md"))?;

// 10〜20行目を読む（1始まり、両端含む）
let excerpt = state.read_file_range(Path::new("notes/hello.md"), 10, Some(20))?;

// ディレクトリを一覧
for (path, is_dir) in state.list_dir(Path::new("notes"))? {
    println!("{} {}", if is_dir { "d" } else { "-" }, path.display());
}
```

### 書き込み／削除（インデックスは自動更新）

```rust
use std::path::Path;

state.write_file(Path::new("notes/hello.md"), "# Hello\n\nworld")?;
state.delete_file(Path::new("notes/old.md"))?;
```

## ワークスペースの検出

ワークスペースルートは、マーカーディレクトリが見つかるまでディレクトリツリーを上へ辿ることで検出されます。すべての構築メソッドに`AppContext`を渡して`app_name`を設定すると、マーカーディレクトリとXDGキャッシュがホストアプリケーションの名前空間を使うようになります。スタートアップで一度`init(AppKind::…)`を呼び出すと、キャッシュ／データ／設定ディレクトリをプラットフォームルート以下のper-binary-typeレイアウト`<プラットフォームルート>/<app-name>/<kind>/`（`kind` = `cli`/`server`/`desktop`、first-writer-wins）に解決し、一回限りのディレクトリ移行を適用します：

```rust
use sapphire_workspace::{AppContext, AppKind};

let ctx = AppContext::new("sapphire-journal");
ctx.init(AppKind::Server);
// marker: {root}/.sapphire-journal/
// cache:  $XDG_CACHE_HOME/sapphire-journal/server/{uuid}/
```

## 安定したワークスペースUUID

各ワークスペースディレクトリは、正規化されたパスのMD5ハッシュから導出される安定した[UUIDv8]識別子を持ちます。UUIDはディスクに保存されず、`Workspace::uuid()`や単体の`path_uuid()`関数の呼び出しごとに再計算されます。

```rust
println!("{}", sapphire_workspace::path_uuid(Path::new("/my/workspace")));
```

## 設定

`config.toml`をマーカーディレクトリ内（`.sapphire-workspace/config.toml`）に置きます：

```toml
[retrieve]
db = "redb"   # "none" | "redb"
```

埋め込みはここでは設定しません。ブリッジが埋め込みを行い（`<bridge dir>/embedding.toml`）、
`WorkspaceState::load_embedder()`がブリッジに問い合わせます。埋め込むブリッジがなければ検索はFTSのみです。

環境変数による上書き：

| 変数 | 値 |
|---|---|
| `SAPPHIRE_WORKSPACE_RETRIEVE_DB` | `none` / `redb` |

## サポートするファイルタイプ

インデクサはワークスペースルートを歩き回り（隠しディレクトリはスキップ）、次を処理します：

| 拡張子 | インデックス |
|---|---|
| `md`, `markdown`, `txt`, `rst`, `org` | 1ファイル = 1ドキュメントとして索引 |
| `toml` | 1ファイル = 1ドキュメントとして索引 |
| `jsonl` | 1ファイル = 1ドキュメントとして索引 |

## ライセンス

[MIT](../LICENSE-MIT)または[Apache-2.0](../LICENSE-APACHE)のうち、選択したいずれか1つでライセンスされます。

[`sapphire-retrieve`]: ../sapphire-retrieve
[UUIDv8]: https://www.rfc-editor.org/rfc/rfc9562#name-uuid-version-8
