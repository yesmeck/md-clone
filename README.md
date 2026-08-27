# md2notion

A Rust CLI that syncs a folder of Markdown files to Notion. Run it as often
as you like, from any machine: the first sync creates one Notion page per
`.md` file, and every later sync updates those same pages in place —
unchanged files are skipped.

There is **no local state**: the mapping between files and pages lives in
Notion itself, in a managed database whose entries carry `Source Path` and
`Content Hash` properties. Any machine with the token can rebuild the full
mapping with a single query, so teams and CI can sync the same folder
without ever creating duplicates.

## Setup

1. Create an internal integration at <https://www.notion.so/my-integrations>
   and copy its secret token.
2. In Notion, open the page you want the documents to live under, then
   **⋯ → Connections → Connect to** your integration (this grants the API
   access to that page and its children).
3. Build the CLI:

   ```sh
   cargo build --release   # binary at target/release/md2notion
   ```

## Usage

```sh
export NOTION_TOKEN=ntn_xxx                 # or pass --token, or use OAuth (below)
export MD2NOTION_PARENT="https://www.notion.so/acme/Docs-1a2b..."   # or pass --parent

md2notion sync ./docs
```

`--parent` accepts either:

- **a page** — the tool finds (or creates, on first run) a managed database
  under it, named after the folder; or
- **a database** — used directly; any missing `Source Path` / `Content Hash`
  / `Last Synced` properties are added to its schema automatically.

Options:

| Flag | Effect |
|---|---|
| `--parent <ID or URL>` | Target page or database. Falls back to `MD2NOTION_PARENT`. |
| `--token <TOKEN>` | Integration token; falls back to `NOTION_TOKEN`. |
| `--dry-run` | Query Notion and print the create/update/skip plan without changing anything. |
| `--force` | Re-upload files even when their content hash is unchanged. |
| `--prune` | Archive Notion pages whose source file has been deleted. |

## OAuth login (optional)

Instead of an integration secret, you can log in through Notion's OAuth
consent screen — its "select pages to share" step replaces manually
connecting the integration to a page. Notion has no device flow, so this
requires a **public integration** of your own:

1. At <https://www.notion.so/my-integrations>, make the integration public
   and copy its **OAuth client ID** and **OAuth client secret**.
2. Add `http://localhost:8237/callback` to its redirect URIs (or pick
   another port and pass `--port`).
3. Log in — the browser opens, you approve, the token is stored in
   `~/.config/md2notion/credentials.json` (mode 0600):

   ```sh
   md2notion login --client-id <ID> --client-secret <SECRET>
   # or via NOTION_OAUTH_CLIENT_ID / NOTION_OAUTH_CLIENT_SECRET
   ```

`sync` resolves its token as: `--token` → `NOTION_TOKEN` → stored login.
`md2notion logout` removes the stored credentials. Notion's OAuth access
tokens are long-lived; there is no refresh flow. CI should keep using
`NOTION_TOKEN` — a headless runner can't do a browser flow.

## How syncing works

- The folder is scanned recursively for `.md` files (hidden files and
  directories are ignored).
- The managed database is discovered by a marker in its *description*
  (`managed by md2notion`), so you can freely rename it or restyle its views.
  If several databases under the parent carry the marker, the sync stops
  with an error rather than guessing.
- Each entry stores the file's relative path, a SHA-256 of its content at
  last sync, and a `Last Synced` timestamp. On each run, one paginated query
  rebuilds the mapping; files are then compared by hash — new files become
  new entries, changed files get their title updated and their blocks
  replaced, unchanged files are skipped.
- Rows deleted by hand in Notion are simply recreated on the next sync. If a
  row was *duplicated* by hand, the oldest one keeps receiving updates and
  the extras are warned about (never auto-archived — they may contain
  someone's edits). Rows added by hand without a `Source Path` are ignored.
- Sync is one-way: markdown is the source of truth, and a file change
  overwrites any manual edits made to its page content in Notion.
- Notion's rate limits are handled with automatic retries that honor
  `Retry-After`.

## Markdown support

- The page title comes from a leading `# H1` (removed from the body), or the
  file name if there is none.
- Headings (H1–H3; deeper levels render as H3), paragraphs, bold, italic,
  strikethrough, inline code, and links.
- Bulleted, numbered, and task lists (`- [ ]` / `- [x]`), including nesting.
  Notion accepts two levels of nesting per request; deeper lists are
  flattened to that depth.
- Fenced code blocks with language mapping (unknown languages fall back to
  plain text), block quotes, horizontal rules, and images with `http(s)`
  URLs (local image files cannot be uploaded through the Notion API).
- Tables, including inline formatting in cells. Column alignment is ignored
  (Notion tables have none), and tables longer than 100 rows are split into
  consecutive table blocks (Notion's per-request limit).
- Not converted: raw HTML blocks and footnotes — their text is either
  rendered as plain paragraphs or skipped.

## Development

```sh
cargo test    # converter, ID parsing, and property-shape tests
cargo clippy
```
