# git-searcher

# GitHub Enterprise Server ファイル検索 & 最新コミット情報取得スクリプト

このリポジトリは、GitHub Enterprise Server (GHES) 環境で動作し、指定したファイル名を含むすべてのリポジトリを検索して、
各リポジトリの指定したファイルに関する最新コミットユーザー名、コミットハッシュ（SHA）、コミット日時、リポジトリ URL を取得・表示する Rust 製スクリプトです。

---

## プロジェクト概要

1. **REST API** で `search/code` を呼び出し、特定のファイル名を含むリポジトリとパスを取得
2. 各リポジトリについて **REST API** で `default_branch` の存在をチェック
3. **GraphQL API** を用いて `defaultBranchRef.history(path: ...)` で最新コミット情報を取得
4. 各リポジトリのリストを取得し、最新コミット情報とリポジトリ URL を表示
5. 処理間にスリープを挟むことで API 過負荷を抑制

## 実装中の懸念事項と対応

1. GHES と GitHub.com の API スキーマの違い

   * GitHub.com では GraphQL の `object(expression:)` を利用してファイルを直接取得可能。
   * しかし **GHES ではスキーマが異なり `object()` が利用不可**。
   * そのため、**REST API (search/code)** で対象リポジトリを検索し、**GraphQL (defaultBranchRef.history(path:))** で最新コミットを取得する方式を採用。

2. 信頼性・エラー回避

   * API レスポンスは `Option<T>` が多く、直接 unwrap するとパニックが発生。
   * 本実装では `.as_ref().and_then(...)` で安全に参照を辿る方式を採用。
   * `None` が返るケースでは明示的に `⚠️` や `❌` をログに出すようにした。

3. API 過負荷防止

   * 大規模リポジトリを横断的に叩くため、GitHub API のレートリミットに配慮。
   * 各リポジトリ処理後に 1 秒のスリープ を入れる実装にしている。


## セットアップ & 実行手順

### 1. GitHub Enterprise 用トークンの準備

* GHES で `repo` スコープを持つ Personal Access Token を発行し、控えておきます。
    * scope: repo, admin:org

### 2. 環境変数設定

* プロジェクトルートに `.env` ファイルを作成し、以下を記述します：

  ```ini
  GHE_URL=https://<your-ghe-domain>
  GITHUB_TOKEN=<your-personal-access-token>
  ```

### 3. 依存クレートの追加

`Cargo.toml` に以下を追加してください：

```toml
[dependencies]
anyhow = "1.0"
dotenv = "0.15"
graphql_client = "0.13"
reqwest = { version = "0.11", features = ["json", "blocking", "tls"] }
serde_json = "1.0"
tokio = { version = "1", features = ["full"] }
```

### 4. ビルド & 実行

```bash
git clone <this-repo>
cd <this-repo>
cargo build --release
# ファイル名を引数に実行
cargo run --release -- slug.yml
```

### 5. 実行結果

実行結果は下記の文字列と組み合わせると、
マークダウン形式でテーブル形式で表示できる。

```
リポジトリ名 | URL | 最終コミットユーザー | コミットハッシュ | コミット日次
-- | -- | -- | -- | --

### サンプル出力
```
🔍 `hoge.yml` を含むリポジトリ: 5 件
📁 hoge/test1 | 🌐 https://ghes.example.com/hoge/test1 | 👤 alice | 🔑 a1b2c3d | 📅 2025-08-21T10:15:30Z
📁 fuga/test2  | 🌐 https://ghes.example.com/fuga/test2  | 👤 bob   | 🔑 d4e5f6g | 📅 2025-08-20T18:47:12Z
...
```

## ファイル構成

```
.gitignore
Cargo.toml
.env             # 環境変数ファイル (ローカル)
src/
  main.rs        # エントリーポイント
  query.rs       # graphql_client derive 用定義
  query.graphql  # 実際の GraphQL クエリ
  dummy.graphql  # ダミースキーマ (コード生成用)
```

## 依存ライブラリ

* [anyhow](https://crates.io/crates/anyhow) : エラーハンドリング
* [dotenv](https://crates.io/crates/dotenv) : `.env` 読み込み
* [graphql-client](https://crates.io/crates/graphql-client) : GraphQL クライアント
* [reqwest](https://crates.io/crates/reqwest) : HTTP クライアント
* [serde\_json](https://crates.io/crates/serde_json) : JSON パース
* [tokio](https://crates.io/crates/tokio) : 非同期ランタイム

## 実装の背景とポイント

1. **GHES の GraphQL スキーマ制約**
   GitHub.com と異なり `repository.object(expression:)` が使えない場合があるため、
   `defaultBranchRef.history(path:)` を利用してファイル単位の履歴を取得しています。

2. **REST + GraphQL の組み合わせ**

   * REST: `search/code` → ファイル名マッチするリポジトリ一覧を取得
   * REST: `repos/{owner}/{repo}` → `default_branch` 存在チェック
   * GraphQL: `history(path:)` → 最新コミット情報取得

3. **Option<T> の安全なアンラップ**
   各 API レスポンスは nullables が多いため、`as_ref()` / `and_then()` / `if let` を駆使し、
   パニックを避ける安全仕様にしています。

4. **API 過負荷対策**
   各リポジトリ処理後に `tokio::time::sleep(Duration::from_secs(1)).await` を挟み、
   高頻度リクエストによるタイムアウトやレート制限を回避します。

5. **BTreeSet による重複排除**
   同じリポジトリ・ファイルパスが重複して取得されないようにしています。

## main.rs 詳細解説

### 1. 環境変数 & クライアント初期化

```rust
dotenv().ok();
let ghe_url = env::var("GHE_URL")?;
let token   = env::var("GITHUB_TOKEN")?;
let rest    = Client::new();
let graphql = Client::new();
```

* `.env` を読み込み、`GHE_URL`, `GITHUB_TOKEN` を取得。
* `reqwest::Client` を REST と GraphQL 両方で使用。

### 2. REST でファイル検索 (search/code)

```rust
let search_url = format!("{}/api/v3/search/code", ghe_url);
let resp = rest.get(&search_url)
    .bearer_auth(&token)
    .query(&[("q", format!("filename:{}", filename)), ("per_page", "100")])
    .send().await?.error_for_status()?;
let body: Value = resp.json().await?;
```

* `filename:<target>` クエリで最大 100 件取得。
* 結果を `serde_json::Value` にデコード。

### 3. (repo\_full, path) の抽出 & 重複排除

```rust
let mut targets = BTreeSet::new();
if let Some(items) = body["items"].as_array() {
  for item in items {
    let repo = item["repository"]["full_name"].as_str();
    let path = item["path"].as_str();
    targets.insert((repo.to_string(), path.to_string()));
  }
}
```

* `BTreeSet` で `("owner/repo", "path")` を一意管理。

### 4. 各リポジトリ処理ループ

```rust
for (repo_full, path) in targets {
  let (owner, repo) = repo_full.split_once('/').unwrap();
  // default_branch 存在チェック (省略可)
  // GraphQL で最新コミット取得 → history.edges.first().node
  // 結果のパース & `println!`
  sleep(Duration::from_secs(1)).await;
}
```

* `owner`, `repo`, `path` を分解。
* `history(first:1, path: path)` で最新コミットを一件だけフェッチ。
* パース後、`login | sha | date | url` を整形して標準出力。
* 各ループ後に 1 秒スリープ。

## まとめ

* GHES 固有の GraphQL 制約に対応するため、REST+GraphQL を組み合わせたハイブリッド実装。
* 多数の `Option<T>` を安全にアンラップし、想定外のレスポンス構造でもクラッシュしない設計。
* API 呼び出し間隔をあけることで安定性を確保。
