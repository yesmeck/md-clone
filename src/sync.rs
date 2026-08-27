use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::markdown;
use crate::notion::{normalize_id, DbEntry, Notion, ParentKind};

pub struct SyncOptions {
    pub dir: PathBuf,
    pub parent: Option<String>,
    pub token: Option<String>,
    pub force: bool,
    pub prune: bool,
    pub dry_run: bool,
}

#[derive(PartialEq)]
enum Action {
    Create,
    Update,
    Skip,
}

struct PlannedFile {
    rel: String,
    content: String,
    hash: String,
    action: Action,
}

pub async fn run(opts: SyncOptions) -> Result<()> {
    let dir = opts
        .dir
        .canonicalize()
        .with_context(|| format!("cannot open {}", opts.dir.display()))?;
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }

    let parent = opts
        .parent
        .context("pass --parent <page or database URL/ID> or set MD2NOTION_PARENT")?;
    let parent_id = normalize_id(&parent)?;
    let token = match opts.token {
        Some(t) => t,
        None => match crate::auth::load_credentials()? {
            Some(creds) => {
                if let Some(ws) = &creds.workspace_name {
                    println!("using stored OAuth credentials (workspace {ws:?})");
                }
                creds.access_token
            }
            None => bail!(
                "no Notion token — pass --token, set NOTION_TOKEN, or run `md2notion login`"
            ),
        },
    };
    let notion = Notion::new(token)?;

    // Resolve the managed database: --parent may be the database itself, or
    // a page under which the marked database is discovered (or created).
    let db_id: Option<String> = match notion.identify(&parent_id).await? {
        ParentKind::Database => Some(parent_id.clone()),
        ParentKind::Page => {
            let marked = notion.find_marked_databases(&parent_id).await?;
            match marked.len() {
                0 => None,
                1 => Some(marked[0].0.clone()),
                _ => {
                    let list = marked
                        .iter()
                        .map(|(id, title)| format!("  {title:?} ({id})"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    bail!(
                        "found {} databases marked as managed by md2notion under this page — \
                         delete all but one, or pass the right one as --parent directly:\n{list}",
                        marked.len()
                    );
                }
            }
        }
    };

    // The database is the only record of past syncs: one query rebuilds the
    // whole path → (page, hash) mapping on any machine.
    let mut entries_by_path: BTreeMap<String, Vec<DbEntry>> = BTreeMap::new();
    if let Some(id) = &db_id {
        if !opts.dry_run {
            notion.ensure_properties(id).await?;
        }
        for e in notion.query_entries(id).await? {
            entries_by_path.entry(e.source_path.clone()).or_default().push(e);
        }
        // Oldest first: on duplicate rows the original keeps receiving
        // updates and the extras are only warned about.
        for v in entries_by_path.values_mut() {
            v.sort_by(|a, b| a.created_time.cmp(&b.created_time));
        }
    }

    let files = find_markdown_files(&dir)?;
    if files.is_empty() {
        println!("no markdown files found in {}", dir.display());
    }

    let mut plan = Vec::new();
    for (rel, path) in &files {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        let action = match entries_by_path.get(rel) {
            Some(v) if v[0].content_hash == hash && !opts.force => Action::Skip,
            Some(_) => Action::Update,
            None => Action::Create,
        };
        plan.push(PlannedFile {
            rel: rel.clone(),
            content,
            hash,
            action,
        });
    }
    let deleted: Vec<String> = entries_by_path
        .keys()
        .filter(|k| !files.iter().any(|(rel, _)| rel == *k))
        .cloned()
        .collect();

    for (path, v) in &entries_by_path {
        if v.len() > 1 && !deleted.contains(path) {
            for extra in &v[1..] {
                eprintln!(
                    "warning: duplicate entry for {path} — syncing the oldest, \
                     please delete {}",
                    extra.url
                );
            }
        }
    }

    if opts.dry_run {
        if db_id.is_none() {
            println!("  would create the managed database under the parent page");
        }
        for f in &plan {
            let label = match f.action {
                Action::Create => "create",
                Action::Update => "update",
                Action::Skip => "skip  ",
            };
            println!("  {label}   {}", f.rel);
        }
        for rel in &deleted {
            let verb = if opts.prune { "archive" } else { "orphan " };
            println!("  {verb}  {rel}");
        }
        println!("dry run: nothing was changed in Notion");
        return Ok(());
    }

    let db_id = match db_id {
        Some(id) => id,
        None => {
            let title = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "md2notion".to_string());
            let (id, url) = notion.create_database(&parent_id, &title).await?;
            println!("created managed database {title:?}  →  {url}");
            id
        }
    };

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (mut created, mut updated, mut skipped) = (0u32, 0u32, 0u32);
    for f in plan {
        if f.action == Action::Skip {
            skipped += 1;
            println!("  unchanged  {}", f.rel);
            continue;
        }

        let converted = markdown::convert(&f.content);
        let title = converted.title.unwrap_or_else(|| fallback_title(&f.rel));

        let page_id = match entries_by_path.get(&f.rel) {
            Some(v) => {
                let entry = &v[0];
                notion.update_entry(&entry.page_id, &title, &f.hash, &now).await?;
                notion.clear_children(&entry.page_id).await?;
                updated += 1;
                println!("  updated    {}", f.rel);
                entry.page_id.clone()
            }
            None => {
                let (id, url) = notion
                    .create_entry(&db_id, &title, &f.rel, &f.hash, &now)
                    .await?;
                created += 1;
                println!("  created    {}  →  {url}", f.rel);
                id
            }
        };
        if !converted.blocks.is_empty() {
            notion.append_children(&page_id, &converted.blocks).await?;
        }
    }

    let mut archived = 0u32;
    for rel in deleted {
        if opts.prune {
            for entry in &entries_by_path[&rel] {
                match notion.archive_page(&entry.page_id).await {
                    Ok(()) => {
                        archived += 1;
                        println!("  archived   {rel}");
                    }
                    Err(e) => eprintln!("warn: could not archive page for {rel}: {e}"),
                }
            }
        } else {
            println!("  missing    {rel} (file deleted; use --prune to archive its Notion page)");
        }
    }

    println!(
        "done: {created} created, {updated} updated, {skipped} unchanged{}",
        if archived > 0 {
            format!(", {archived} archived")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// All .md files under `dir`, as (relative path with forward slashes,
/// absolute path), sorted for stable output. Hidden files and directories
/// are skipped.
fn find_markdown_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(dir).sort_by_file_name() {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let is_md = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !is_md {
            continue;
        }
        let rel = entry.path().strip_prefix(dir)?;
        if rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            continue;
        }
        files.push((
            rel.to_string_lossy().replace('\\', "/"),
            entry.path().to_path_buf(),
        ));
    }
    Ok(files)
}

/// Page title when the document has no leading H1: the file name without
/// directories or the .md extension.
fn fallback_title(rel: &str) -> String {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.strip_suffix(".md")
        .or_else(|| name.strip_suffix(".MD"))
        .unwrap_or(name)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_title_strips_dirs_and_extension() {
        assert_eq!(fallback_title("notes/deep/my-file.md"), "my-file");
        assert_eq!(fallback_title("README.md"), "README");
    }
}
