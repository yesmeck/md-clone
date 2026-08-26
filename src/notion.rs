use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Map, Value};

const API: &str = "https://api.notion.com/v1";
const NOTION_VERSION: &str = "2022-06-28";
const MAX_RETRIES: u32 = 5;
/// Notion accepts at most 100 blocks per append request.
const APPEND_CHUNK: usize = 100;

/// Written into the managed database's description so the tool can find its
/// own database again on any machine, with no local state. Users may rename
/// the database freely; only this marker matters.
pub const DB_MARKER: &str = "managed by md2notion";

pub const PROP_SOURCE_PATH: &str = "Source Path";
pub const PROP_CONTENT_HASH: &str = "Content Hash";
pub const PROP_LAST_SYNCED: &str = "Last Synced";

/// A non-2xx response from the Notion API.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Notion API error {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>()
        .map(|api| api.status == 404)
        .unwrap_or(false)
}

pub enum ParentKind {
    Database,
    Page,
}

/// One row of the managed database, as needed by the sync algorithm.
#[derive(Debug, Clone)]
pub struct DbEntry {
    pub page_id: String,
    pub source_path: String,
    pub content_hash: String,
    /// ISO 8601, lexicographically sortable; used for oldest-wins on
    /// duplicate rows.
    pub created_time: String,
    pub url: String,
}

pub struct Notion {
    http: Client,
    token: String,
}

impl Notion {
    pub fn new(token: String) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { http, token })
    }

    /// Send a request, retrying on 429 (honoring Retry-After) and 5xx.
    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{API}{path}");
        let mut attempt: u32 = 0;
        loop {
            let mut req = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&self.token)
                .header("Notion-Version", NOTION_VERSION);
            if let Some(b) = &body {
                req = req.json(b);
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if attempt < MAX_RETRIES => {
                    attempt += 1;
                    eprintln!("warn: request failed ({e}), retrying...");
                    tokio::time::sleep(Duration::from_secs(2u64.pow(attempt.min(4)))).await;
                    continue;
                }
                Err(e) => return Err(e).context(format!("{method} {path} failed")),
            };

            let status = resp.status();
            if status.is_success() {
                return resp
                    .json()
                    .await
                    .with_context(|| format!("{method} {path}: invalid JSON response"));
            }

            if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                && attempt < MAX_RETRIES
            {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or_else(|| f64::from(2u32.pow(attempt.min(4))));
                attempt += 1;
                tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.5, 60.0))).await;
                continue;
            }

            let text = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["message"].as_str().map(String::from))
                .unwrap_or(text);
            return Err(ApiError {
                status: status.as_u16(),
                message,
            }
            .into());
        }
    }

    /// Is the given ID a database or a plain page?
    pub async fn identify(&self, id: &str) -> Result<ParentKind> {
        match self
            .request(Method::GET, &format!("/databases/{id}"), None)
            .await
        {
            Ok(_) => Ok(ParentKind::Database),
            Err(e) if is_not_found(&e) => {
                match self.request(Method::GET, &format!("/pages/{id}"), None).await {
                    Ok(_) => Ok(ParentKind::Page),
                    Err(e2) if is_not_found(&e2) => bail!(
                        "{id} is neither a page nor a database the integration can see — \
                         check the ID and make sure the page is shared with the integration \
                         (⋯ → Connections)"
                    ),
                    Err(e2) => Err(e2),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Child databases of `page_id` whose description carries [`DB_MARKER`],
    /// as (database_id, title).
    pub async fn find_marked_databases(&self, page_id: &str) -> Result<Vec<(String, String)>> {
        let mut found = Vec::new();
        for block in self.list_children(page_id).await? {
            if block["type"] != "child_database" {
                continue;
            }
            let Some(db_id) = block["id"].as_str() else {
                continue;
            };
            let db = match self
                .request(Method::GET, &format!("/databases/{db_id}"), None)
                .await
            {
                Ok(db) => db,
                // A child_database block can be a linked view of a database
                // that isn't shared with the integration; skip those.
                Err(e) if is_not_found(&e) => continue,
                Err(e) => return Err(e),
            };
            if db["archived"].as_bool().unwrap_or(false)
                || db["in_trash"].as_bool().unwrap_or(false)
            {
                continue;
            }
            let description = rich_text_plain(&db["description"]);
            if description.contains(DB_MARKER) {
                let title = rich_text_plain(&db["title"]);
                found.push((db_id.to_string(), title));
            }
        }
        Ok(found)
    }

    /// Create the managed database under a parent page; returns (id, url).
    pub async fn create_database(&self, page_id: &str, title: &str) -> Result<(String, String)> {
        let body = json!({
            "parent": { "type": "page_id", "page_id": page_id },
            "title": [text_span(title)],
            "description": [text_span(DB_MARKER)],
            "is_inline": true,
            "properties": {
                "Name": { "title": {} },
                PROP_SOURCE_PATH: { "rich_text": {} },
                PROP_CONTENT_HASH: { "rich_text": {} },
                PROP_LAST_SYNCED: { "date": {} },
            },
        });
        let v = self.request(Method::POST, "/databases", Some(body)).await?;
        let id = v["id"]
            .as_str()
            .context("create database: response missing id")?
            .to_string();
        let url = v["url"].as_str().unwrap_or_default().to_string();
        Ok((id, url))
    }

    /// Make sure the database has the properties the sync relies on, adding
    /// any that are missing (someone deleting a column must not brick the
    /// sync — worst case everything re-uploads once and heals).
    pub async fn ensure_properties(&self, db_id: &str) -> Result<()> {
        let db = self
            .request(Method::GET, &format!("/databases/{db_id}"), None)
            .await?;
        let existing = db["properties"].as_object().cloned().unwrap_or_default();
        let mut missing = Map::new();
        for (name, def) in [
            (PROP_SOURCE_PATH, json!({ "rich_text": {} })),
            (PROP_CONTENT_HASH, json!({ "rich_text": {} })),
            (PROP_LAST_SYNCED, json!({ "date": {} })),
        ] {
            if !existing.contains_key(name) {
                missing.insert(name.to_string(), def);
            }
        }
        if !missing.is_empty() {
            let names: Vec<&String> = missing.keys().collect();
            eprintln!("note: adding missing propert{} {:?} to the database", if names.len() == 1 { "y" } else { "ies" }, names);
            let body = json!({ "properties": Value::Object(missing) });
            self.request(Method::PATCH, &format!("/databases/{db_id}"), Some(body))
                .await?;
        }
        Ok(())
    }

    /// All rows of the managed database. Rows without a Source Path (added
    /// by humans) are skipped and left alone.
    pub async fn query_entries(&self, db_id: &str) -> Result<Vec<DbEntry>> {
        let mut entries = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = json!({ "page_size": 100 });
            if let Some(c) = &cursor {
                body["start_cursor"] = json!(c);
            }
            let v = self
                .request(Method::POST, &format!("/databases/{db_id}/query"), Some(body))
                .await?;
            if let Some(results) = v["results"].as_array() {
                for page in results {
                    let source_path = rich_text_plain(&page["properties"][PROP_SOURCE_PATH]["rich_text"]);
                    if source_path.is_empty() {
                        continue;
                    }
                    entries.push(DbEntry {
                        page_id: page["id"].as_str().unwrap_or_default().to_string(),
                        source_path,
                        content_hash: rich_text_plain(
                            &page["properties"][PROP_CONTENT_HASH]["rich_text"],
                        ),
                        created_time: page["created_time"].as_str().unwrap_or_default().to_string(),
                        url: page["url"].as_str().unwrap_or_default().to_string(),
                    });
                }
            }
            if v["has_more"].as_bool() == Some(true) {
                cursor = v["next_cursor"].as_str().map(String::from);
                if cursor.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(entries)
    }

    /// Create a database row for a source file; returns (page_id, url).
    pub async fn create_entry(
        &self,
        db_id: &str,
        title: &str,
        source_path: &str,
        hash: &str,
        now: &str,
    ) -> Result<(String, String)> {
        let body = json!({
            "parent": { "type": "database_id", "database_id": db_id },
            "properties": entry_properties(title, Some(source_path), hash, now),
        });
        let v = self.request(Method::POST, "/pages", Some(body)).await?;
        let id = v["id"]
            .as_str()
            .context("create entry: response missing id")?
            .to_string();
        let url = v["url"].as_str().unwrap_or_default().to_string();
        Ok((id, url))
    }

    pub async fn update_entry(
        &self,
        page_id: &str,
        title: &str,
        hash: &str,
        now: &str,
    ) -> Result<()> {
        let body = json!({ "properties": entry_properties(title, None, hash, now) });
        self.request(Method::PATCH, &format!("/pages/{page_id}"), Some(body))
            .await?;
        Ok(())
    }

    pub async fn archive_page(&self, page_id: &str) -> Result<()> {
        let body = json!({ "archived": true });
        self.request(Method::PATCH, &format!("/pages/{page_id}"), Some(body))
            .await?;
        Ok(())
    }

    /// Delete all existing content blocks of a page (used before re-uploading
    /// the converted markdown on an update).
    pub async fn clear_children(&self, page_id: &str) -> Result<()> {
        for block in self.list_children(page_id).await? {
            if let Some(id) = block["id"].as_str() {
                self.request(Method::DELETE, &format!("/blocks/{id}"), None)
                    .await?;
            }
        }
        Ok(())
    }

    async fn list_children(&self, block_id: &str) -> Result<Vec<Value>> {
        let mut blocks = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let path = match &cursor {
                Some(c) => format!("/blocks/{block_id}/children?page_size=100&start_cursor={c}"),
                None => format!("/blocks/{block_id}/children?page_size=100"),
            };
            let v = self.request(Method::GET, &path, None).await?;
            if let Some(results) = v["results"].as_array() {
                blocks.extend(results.iter().cloned());
            }
            if v["has_more"].as_bool() == Some(true) {
                cursor = v["next_cursor"].as_str().map(String::from);
                if cursor.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(blocks)
    }

    pub async fn append_children(&self, page_id: &str, blocks: &[Value]) -> Result<()> {
        for chunk in blocks.chunks(APPEND_CHUNK) {
            let body = json!({ "children": chunk });
            self.request(
                Method::PATCH,
                &format!("/blocks/{page_id}/children"),
                Some(body),
            )
            .await?;
        }
        Ok(())
    }
}

/// Page properties for a synced entry. The title property is addressed by
/// its fixed id "title" so a renamed title column still works. Source Path
/// is only written on create — it is the identity of the row.
fn entry_properties(title: &str, source_path: Option<&str>, hash: &str, now: &str) -> Value {
    let mut props = json!({
        "title": { "title": [text_span(title)] },
        PROP_CONTENT_HASH: { "rich_text": [text_span(hash)] },
        PROP_LAST_SYNCED: { "date": { "start": now } },
    });
    if let Some(path) = source_path {
        props[PROP_SOURCE_PATH] = json!({ "rich_text": [text_span(path)] });
    }
    props
}

fn text_span(content: &str) -> Value {
    // Notion caps a rich text span at 2000 characters.
    let truncated: String = content.chars().take(2000).collect();
    json!({ "type": "text", "text": { "content": truncated } })
}

/// Concatenated plain text of a rich text array.
fn rich_text_plain(rich: &Value) -> String {
    rich.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    s["plain_text"]
                        .as_str()
                        .or_else(|| s["text"]["content"].as_str())
                })
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Accept a page/database ID (dashed or not) or a full Notion URL and return
/// the canonical dashed UUID.
pub fn normalize_id(input: &str) -> Result<String> {
    let input = input.trim();
    // Drop any query string / fragment, then take the last path segment.
    let no_query = input.split(['?', '#']).next().unwrap_or(input);
    let segment = no_query.rsplit('/').next().unwrap_or(no_query);
    let cleaned: String = segment.chars().filter(|c| *c != '-').collect();

    // The ID is the trailing run of 32 hex characters (Notion URLs put it at
    // the end of the slug).
    let hex_suffix: String = cleaned
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if hex_suffix.len() < 32 {
        bail!("could not find a Notion page or database ID in {input:?}");
    }
    let id = &hex_suffix[hex_suffix.len() - 32..];
    let id = id.to_lowercase();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &id[0..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DASHED: &str = "1a2b3c4d-5e6f-7a8b-9c0d-1e2f3a4b5c6d";

    #[test]
    fn accepts_dashed_id() {
        assert_eq!(normalize_id(DASHED).unwrap(), DASHED);
    }

    #[test]
    fn accepts_undashed_id() {
        assert_eq!(
            normalize_id("1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d").unwrap(),
            DASHED
        );
    }

    #[test]
    fn accepts_notion_url() {
        let url = "https://www.notion.so/acme/My-Page-1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d?pvs=4";
        assert_eq!(normalize_id(url).unwrap(), DASHED);
    }

    #[test]
    fn rejects_garbage() {
        assert!(normalize_id("not-a-page").is_err());
    }

    #[test]
    fn rich_text_plain_reads_query_results_and_own_spans() {
        let from_query = json!([{ "plain_text": "a/b.md", "text": { "content": "ignored" } }]);
        assert_eq!(rich_text_plain(&from_query), "a/b.md");
        let own = json!([text_span("hello "), text_span("world")]);
        assert_eq!(rich_text_plain(&own), "hello world");
    }

    #[test]
    fn entry_properties_only_write_path_on_create() {
        let with_path = entry_properties("T", Some("a.md"), "h", "2026-01-01T00:00:00Z");
        assert_eq!(
            with_path[PROP_SOURCE_PATH]["rich_text"][0]["text"]["content"],
            "a.md"
        );
        let without = entry_properties("T", None, "h", "2026-01-01T00:00:00Z");
        assert!(without.get(PROP_SOURCE_PATH).is_none());
    }
}
