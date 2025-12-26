/* main.rs
    GitHub Enterprise Server (GHES) 上の全リポジトリから指定ファイルを含むリポジトリを検索し、
    該当ファイルに対する最新コミットのユーザー名、SHA、コミット日時、リポジトリ URL を取得・表示するスクリプト
    実装の背景:
    - GHES 環境では GraphQL スキーマが GitHub.com と異なり object() が使えないため
      REST でファイル検索、GraphQL で defaultBranchRef.history(path:) を利用
    - 複数の Option<T> を安全にアンラップすることでパニックやエラーを回避
    - API 過負荷対策として各リポジトリ処理後にスリープを挿入
*/ 

use anyhow::{Context, Result};                      // エラー伝播を簡潔に扱うため
use dotenv::dotenv;                                 // .env ファイルから環境変数をロード
use graphql_client::{GraphQLQuery, Response};       // graphql_client derive 用
use reqwest::Client;                                // HTTP リクエスト用
use serde_json::Value;                              // REST レスポンス JSON パース用
use std::{collections::BTreeSet, env};              // リポジトリセットと env 参照用
use tokio::time::{sleep, Duration};                 // 非同期スリープ

mod query;                                          // GraphQL クエリ定義を保持するモジュール
use crate::query::FileBlame;                        // GraphQLQuery derive された構造体
use crate::query::file_blame::{                     
    Variables,                                      // クエリ変数型
    ResponseData,                                   // レスポンスデータ型
    FileBlameRepositoryDefaultBranchRefTarget,      // defaultBranchRef.target の enum
};
///--------------------------------------
/// 設定値をまとめる構造体
///--------------------------------------
struct Config {
    ghe_url: String,
    token: String,
    filename: String,
    graphql_url: String,
}

/// 指定ファイルが見つかった (リポジトリ, パス) を表す
#[derive(Debug, Clone)]
struct RepoTarget {
    owner: String,
    repo: String,
    path: String,
}

/// 表示用に整えたコミット情報
#[derive(Debug, Clone)]
struct CommitInfo {
    repo_full: String,
    url: String,
    login: String,
    sha: String,
    date: String,
}

///--------------------------------------
/// 入口: 環境・引数の読み込み
///--------------------------------------
fn load_config() -> Result<Config> {
    dotenv().ok();

    // GHE_URL: GHES のベース URL
    let ghe_url = env::var("GHE_URL").context("環境変数 GHE_URL が設定されていません")?;
    // GITHUB_TOKEN: 認証用トークン
    let token   = env::var("GITHUB_TOKEN").context("環境変数 GITHUB_TOKEN が設定されていません")?;
    // 実行時引数で検索対象のファイル名を取得
    let filename = env::args()
        .nth(1)
        .context("Usage: cargo run -- <filename>")?;
    // GraphQL エンドポイントの URL (GHES 固有)
    let graphql_url = format!("{}/api/graphql", ghe_url.trim_end_matches('/'));

    Ok(Config { ghe_url, token, filename, graphql_url })
}

///--------------------------------------
/// REST: /search/code で filename マッチを全ページ走査
/// - 戻り値は重複を排した RepoTarget のベクタ
///--------------------------------------
async fn search_repos_with_file(
    rest: &Client,
    cfg: &Config,
) -> Result<Vec<RepoTarget>> {
    let mut set: BTreeSet<(String, String)> = BTreeSet::new(); // (repo_full, path)

    // GHES の search API は GitHub.com と同様に利用可能
    let search_url = format!("{}/api/v3/search/code", cfg.ghe_url.trim_end_matches('/'));
    let mut page = 1usize;

    loop {
        let resp = rest
            .get(&search_url)
            .bearer_auth(&cfg.token)
            .query(&[
                ("q", format!("filename:{}", cfg.filename)),
                ("per_page", "100".to_string()),
                ("page", page.to_string()),
            ])
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("search/code(page={}) の呼び出しに失敗", page))?;

        // JSON 文字列を serde_json::Value にデコード
        let body: Value = resp.json().await
            .context("search/code の JSON パースに失敗")?;

        let items = body["items"].as_array().cloned().unwrap_or_default();
        if items.is_empty() {
            break; // ページ終端
        }

        for item in items {
            if let (Some(repo_full), Some(path)) = (
                item["repository"]["full_name"].as_str(),
                item["path"].as_str(),
            ) {
                set.insert((repo_full.to_string(), path.to_string()));
            }
        }

        page += 1;
        // ページまたぎの過負荷対策
        sleep(Duration::from_millis(250)).await;
    }

    let targets = set.into_iter().map(|(repo_full, path)| {
        let (owner, repo) = repo_full
            .split_once('/')
            .expect("Invalid repo format");
        RepoTarget { owner: owner.to_string(), repo: repo.to_string(), path }
    }).collect();

    Ok(targets)
}

///--------------------------------------
/// REST: /repos/{owner}/{repo} で default_branch 確認（任意）
/// - なくても GraphQL は動くことが多いが、健全性チェックとして保持
///--------------------------------------
async fn ensure_repo_info(
    rest: &Client,
    cfg: &Config,
    target: &RepoTarget,
) -> Result<()> {
    let url = format!(
        "{}/api/v3/repos/{}/{}",
        cfg.ghe_url.trim_end_matches('/'),
        target.owner,
        target.repo
    );

    // 基本使わないが API レベルでのリポジトリ確認
    let info: Value = rest
        .get(&url)
        .bearer_auth(&cfg.token)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("GET {} に失敗", url))?
        .json()
        .await
        .context("repo info JSON パースに失敗")?;

    if info["default_branch"].is_null() {
        // ここでは警告に留める（GraphQL で defaultBranchRef がなくても safe にハンドリング）
        eprintln!("⚠️ default_branch が取得できません: {}/{}", target.owner, target.repo);
    }
    Ok(())
}

///--------------------------------------
/// GraphQL: 指定 path の最新コミット 1 件を取得
/// - 成功時は CommitInfo を返す
/// - defaultBranchRef がない/履歴がない等は Ok(None)
///--------------------------------------
async fn fetch_latest_commit_for_path(
    graphql: &Client,
    cfg: &Config,
    target: &RepoTarget,
) -> Result<Option<CommitInfo>> {
    // GraphQL 変数
    let variables = Variables {
        owner: target.owner.clone(),
        repo:  target.repo.clone(),
        path:  target.path.clone(),
    };

    // 下記の処理でくGrapnQLのクエリに variables を渡しており、
    // GraphQL内の下記の処理内のhistory フィールドには path 引数を渡せる仕様があります。
    // これにより、指定したファイルに対するコミット履歴だけがフィルタされる。
    // first: 1 にしているので、そのファイルを最後に更新したコミットが1件だけ返ってくる。
    // 以降はその情報に対して、コミット日時やユーザーを取得していく
    // 
    // history(path: $path, first: 1) {

    let req_body = FileBlame::build_query(variables);

    let res = graphql
        .post(&cfg.graphql_url)
        .bearer_auth(&cfg.token)
        .json(&req_body)
        .send()
        .await
        .with_context(|| format!("GraphQL POST 失敗: {}/{}", target.owner, target.repo))?;

    let response_body: Response<ResponseData> = res
        .json()
        .await
        .context("GraphQL レスポンス JSON パースに失敗")?;

    // repository が None のときは情報不足として None
    let Some(repo_data) = response_body
        .data
        .as_ref()
        .and_then(|d| d.repository.as_ref())
    else {
        eprintln!("⚠️ GraphQL repository null: {}/{}", target.owner, target.repo);
        return Ok(None);
    };

    //  defaultBranchRef.target → Commit 取得
    let Some(commit_target) = repo_data
        .default_branch_ref
        .as_ref()
        .and_then(|r| r.target.as_ref())
    else {
        eprintln!("⚠️ defaultBranchRef.target なし: {}/{}", target.owner, target.repo);
        return Ok(None);
    };

    // enum から Commit 以外は来ない想定（来たら None）
    let commit = match commit_target {
        FileBlameRepositoryDefaultBranchRefTarget::Commit(c) => c,
    };

    // history(path: $path, first: 1) の node を読む
    // Commit.history.edges → 最新コミットノードを取得
    let node = commit
        .history
        .as_ref()
        .and_then(|h| h.edges.as_ref())
        .and_then(|edges| edges.first())
        .and_then(|edge_opt| edge_opt.as_ref())
        .and_then(|edge| edge.node.as_ref());

    let Some(node) = node else {
        eprintln!("⚠️ history.edges.node なし: {}/{}", target.owner, target.repo);
        return Ok(None);
    };

    // CommitNode からユーザー名・SHA・日付を取り出し
    // login 優先、なければ author.name
    let login = node.author
        .as_ref()
        .and_then(|a| a.user.as_ref())
        .and_then(|u| u.login.as_ref())
        .map(|s| s.to_string())
        .or_else(|| node.author.as_ref().and_then(|a| a.name.clone()))
        .unwrap_or_else(|| "unknown".to_string());

    let sha  = node.abbreviated_oid.as_deref().unwrap_or("-").to_string();
    let date = node.committed_date.as_deref().unwrap_or("-").to_string();

    let repo_full = format!("{}/{}", target.owner, target.repo);
    let url = format!("{}/{}/{}", cfg.ghe_url.trim_end_matches('/'), target.owner, target.repo);

    Ok(Some(CommitInfo { repo_full, url, login, sha, date }))
}

///--------------------------------------
/// 表示（I/O は最後にまとめる）
///--------------------------------------
fn print_commit(info: &CommitInfo) {
    println!(
        "📁 {} | 🌐 {} | 👤 {} | 🔑 {} | 📅 {}",
        info.repo_full, info.url, info.login, info.sha, info.date
    );
}

///--------------------------------------
/// メイン
///--------------------------------------
#[tokio::main]
async fn main() -> Result<()> {
    let cfg = load_config()?;

    // クライアントは生成コストが高いので 1 度だけ
    let rest    = Client::new();
    let graphql = Client::new();

    // 1) ファイルを含むリポジトリを検索（全ページ）
    let targets = search_repos_with_file(&rest, &cfg).await?;
    println!("🔍 `{}` を含むリポジトリ: {} 件", cfg.filename, targets.len());

    // 2) 各リポジトリごとに GraphQL で最新コミットを取得
    for target in targets {
        // 健全性チェック（任意）
        let _ = ensure_repo_info(&rest, &cfg, &target).await;

        match fetch_latest_commit_for_path(&graphql, &cfg, &target).await {
            Ok(Some(info)) => print_commit(&info),
            Ok(None) => {
                // ファイルが defaultBranch になかった・履歴が空 など
                println!("⚠️ 該当コミットが見つかりません: {}/{}", target.owner, target.repo);
            }
            Err(e) => {
                eprintln!("❌ 取得失敗 {}/{}: {:?}", target.owner, target.repo, e);
            }
        }

        // 過負荷対策（必要に応じて調整）
        sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}
