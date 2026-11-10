use anyhow::Context;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, body::Bytes};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::database::changelog_repository::{self, ChangelogState, NewChangelogEntry};

const COMMITS_PER_PAGE: usize = 100;
const MAX_HISTORY_PAGES: usize = 100;
const DEFAULT_REPOSITORY: &str = "GeneralErwin/Discord-Jailbot";

type GithubHttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

#[derive(Debug, Clone, Deserialize)]
struct GithubCommit {
    sha: String,
    html_url: String,
    #[serde(default)]
    commit: GithubCommitDetails,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GithubCommitDetails {
    #[serde(default)]
    message: String,
    #[serde(default)]
    committer: GithubCommitter,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GithubCommitter {
    #[serde(default)]
    date: String,
}

fn make_client() -> GithubHttpClient {
    let connector = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .build();
    Client::builder(TokioExecutor::new()).build(connector)
}

fn repository() -> anyhow::Result<String> {
    let repository =
        std::env::var("GITHUB_REPOSITORY").unwrap_or_else(|_| DEFAULT_REPOSITORY.to_owned());
    let mut parts = repository.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        anyhow::bail!("GITHUB_REPOSITORY must use owner/repository format");
    }
    Ok(format!("{owner}/{name}"))
}

async fn fetch_page(
    client: &GithubHttpClient,
    repo: &str,
    page: usize,
) -> anyhow::Result<Vec<GithubCommit>> {
    let uri = format!(
        "https://api.github.com/repos/{repo}/commits?per_page={COMMITS_PER_PAGE}&page={page}"
    );
    let mut request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header("User-Agent", "discord-jailbot-github-changelog")
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .body(Full::new(Bytes::new()))?;

    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.trim().is_empty() {
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {}", token.trim()).parse()?);
        }
    }

    let response = client.request(request).await?;
    let status = response.status();
    let body = response.into_body().collect().await?.to_bytes();
    if status != StatusCode::OK {
        let reason = String::from_utf8_lossy(&body);
        anyhow::bail!(
            "GitHub returned HTTP {status}: {}",
            reason.chars().take(300).collect::<String>()
        );
    }
    Ok(serde_json::from_slice(&body)?)
}

/// Synchronizes the repository's default branch commit history into persistent versioned entries.
pub async fn sync(pool: &SqlitePool) -> anyhow::Result<usize> {
    let repo = repository()?;
    let client = make_client();
    let previous_state = changelog_repository::get_state(pool).await?;
    let mut unseen_newest_first = Vec::new();
    let mut found_cursor = previous_state.is_none();

    for page_number in 1..=MAX_HISTORY_PAGES {
        let page = fetch_page(&client, &repo, page_number).await?;
        if page.is_empty() {
            break;
        }

        if let Some(previous) = &previous_state {
            if let Some(cursor_index) = page
                .iter()
                .position(|commit| commit.sha == previous.last_sha)
            {
                unseen_newest_first.extend(page[..cursor_index].iter().cloned());
                found_cursor = true;
                break;
            }
        }

        let is_last_page = page.len() < COMMITS_PER_PAGE;
        unseen_newest_first.extend(page);
        if is_last_page {
            break;
        }
        if page_number == MAX_HISTORY_PAGES {
            anyhow::bail!(
                "GitHub history exceeded {MAX_HISTORY_PAGES} pages; set GITHUB_TOKEN or reduce repository history before bootstrapping the changelog"
            );
        }
    }

    if !found_cursor {
        tracing::warn!(repository = %repo, "Saved changelog commit was not found in GitHub history; treating the visible history as new commits");
    }

    if unseen_newest_first.is_empty() {
        return Ok(0);
    }
    unseen_newest_first.reverse();

    let mut major = previous_state
        .as_ref()
        .map_or(1, |state| state.version_major);
    let mut minor = previous_state
        .as_ref()
        .map_or(0, |state| state.version_minor);
    let mut entries = Vec::with_capacity(unseen_newest_first.len());

    for (index, commit) in unseen_newest_first.iter().enumerate() {
        if previous_state.is_none() && index == 0 {
            major = 1;
            minor = 0;
        } else if is_major_change(&commit.commit.message) {
            major = major.saturating_add(1);
            minor = 0;
        } else {
            minor = minor.saturating_add(1);
        }

        entries.push(NewChangelogEntry {
            sha: &commit.sha,
            version_major: major,
            version_minor: minor,
            message: &commit.commit.message,
            html_url: &commit.html_url,
            committed_at: &commit.commit.committer.date,
        });
    }

    let last = unseen_newest_first
        .last()
        .context("GitHub commit page was empty")?;
    let next_state = ChangelogState {
        last_sha: last.sha.clone(),
        version_major: major,
        version_minor: minor,
    };
    let inserted = entries.len();
    changelog_repository::save_sync(pool, &entries, &next_state).await?;
    tracing::info!(repository = %repo, inserted, version = %format!("{major}.{minor}"), "Synchronized GitHub changelog");
    Ok(inserted)
}

fn is_major_change(message: &str) -> bool {
    let subject = message
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    subject.starts_with("major:")
        || subject.starts_with("major(")
        || subject.contains("!:")
        || message.to_ascii_lowercase().contains("breaking change")
}
