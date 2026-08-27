//! Converts Markdown into Notion block JSON (the `children` payload of the
//! append-block-children endpoint).

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use serde_json::{json, Map, Value};

/// Notion caps a single rich text span at 2000 characters.
const MAX_TEXT_LEN: usize = 2000;
/// Notion allows at most two levels of nested children per append request;
/// anything deeper gets flattened to the deepest allowed level.
const MAX_NESTING: usize = 2;
/// Notion accepts at most 100 children per block in one request, so longer
/// tables are split into consecutive table blocks.
const MAX_TABLE_ROWS: usize = 100;

pub struct Converted {
    /// Text of a leading H1, if the document starts with one. The sync layer
    /// uses it as the page title and it is removed from `blocks`.
    pub title: Option<String>,
    pub blocks: Vec<Value>,
}

pub fn convert(md: &str) -> Converted {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);

    let mut conv = Conv::default();
    for ev in Parser::new_ext(md, opts) {
        conv.event(ev);
    }
    conv.flush_rich();

    let mut blocks = flatten_deep(conv.top, 0);
    let title = extract_title(&mut blocks);
    Converted { title, blocks }
}

#[derive(Default, Clone)]
struct Styles {
    bold: bool,
    italic: bool,
    strikethrough: bool,
    code: bool,
    link: Option<String>,
}

enum FrameKind {
    Item { ordered: bool },
    Quote,
}

/// An open container (list item or block quote). `text` holds the
/// container's own rich text (its first paragraph); further blocks inside it
/// become `children`.
struct Frame {
    kind: FrameKind,
    text: Vec<Value>,
    children: Vec<Value>,
    todo: Option<bool>,
}

/// A table being collected. `cells` belongs to the row currently open.
struct TableState {
    width: usize,
    rows: Vec<Value>,
    cells: Vec<Value>,
}

#[derive(Default)]
struct Conv {
    top: Vec<Value>,
    frames: Vec<Frame>,
    rich: Vec<Value>,
    styles: Styles,
    list_ordered: Vec<bool>,
    /// (language, accumulated text) while inside a code block.
    code: Option<(String, String)>,
    table: Option<TableState>,
    in_image: bool,
}

impl Conv {
    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if self.in_image {
                    return;
                }
                if let Some((_, buf)) = &mut self.code {
                    buf.push_str(&t);
                    return;
                }
                self.push_text(&t, false);
            }
            Event::Code(t) => {
                if !self.in_image {
                    self.push_text(&t, true);
                }
            }
            Event::InlineHtml(t) => self.push_text(&t, false),
            Event::SoftBreak => self.push_text(" ", false),
            Event::HardBreak => self.push_text("\n", false),
            Event::Rule => {
                self.push_block(json!({"object": "block", "type": "divider", "divider": {}}))
            }
            Event::TaskListMarker(checked) => {
                if let Some(f) = self.frames.last_mut() {
                    f.todo = Some(checked);
                }
            }
            // Block-level HTML, footnotes, math: not representable, skipped.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph | Tag::Heading { .. } => self.flush_rich(),
            Tag::CodeBlock(kind) => {
                self.flush_rich();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                // A tight item's text has no paragraph wrapper, so it may
                // still be pending when a nested list begins.
                self.flush_rich();
                self.list_ordered.push(start.is_some());
            }
            Tag::Item => {
                let ordered = self.list_ordered.last().copied().unwrap_or(false);
                self.frames.push(Frame {
                    kind: FrameKind::Item { ordered },
                    text: Vec::new(),
                    children: Vec::new(),
                    todo: None,
                });
            }
            Tag::Table(alignments) => {
                // Notion tables have no per-column alignment; it is ignored.
                self.flush_rich();
                self.table = Some(TableState {
                    width: alignments.len().max(1),
                    rows: Vec::new(),
                    cells: Vec::new(),
                });
            }
            Tag::BlockQuote(_) => {
                self.flush_rich();
                self.frames.push(Frame {
                    kind: FrameKind::Quote,
                    text: Vec::new(),
                    children: Vec::new(),
                    todo: None,
                });
            }
            Tag::Emphasis => self.styles.italic = true,
            Tag::Strong => self.styles.bold = true,
            Tag::Strikethrough => self.styles.strikethrough = true,
            Tag::Link { dest_url, .. } => {
                // Notion rejects relative paths and #anchors as link URLs;
                // render those links as plain text instead.
                let url = dest_url.to_string();
                self.styles.link = is_linkable(&url).then_some(url);
            }
            Tag::Image { dest_url, .. } => {
                self.in_image = true;
                let url = dest_url.to_string();
                // Notion only accepts externally hosted images; local paths
                // cannot be uploaded through the public blocks API.
                if url.starts_with("http://") || url.starts_with("https://") {
                    self.push_block(json!({
                        "object": "block",
                        "type": "image",
                        "image": { "type": "external", "external": { "url": url } },
                    }));
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_rich(),
            TagEnd::Heading(level) => {
                let rich = std::mem::take(&mut self.rich);
                let l = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    _ => 3,
                };
                let key = format!("heading_{l}");
                let mut block = json!({"object": "block", "type": key});
                block[key.as_str()] = json!({ "rich_text": rich });
                self.push_block(block);
            }
            TagEnd::CodeBlock => {
                if let Some((lang, mut text)) = self.code.take() {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    let plain = Styles::default();
                    let rich: Vec<Value> = chunk_str(&text, MAX_TEXT_LEN)
                        .into_iter()
                        .map(|c| make_span(c, &plain))
                        .collect();
                    self.push_block(json!({
                        "object": "block",
                        "type": "code",
                        "code": { "rich_text": rich, "language": notion_language(&lang) },
                    }));
                }
            }
            TagEnd::List(_) => {
                self.list_ordered.pop();
            }
            TagEnd::Item => {
                // Tight lists emit item text without paragraph events, so any
                // pending rich text belongs to this item.
                self.flush_rich();
                let f = match self.frames.pop() {
                    Some(f) => f,
                    None => return,
                };
                let key = match (f.todo, &f.kind) {
                    (Some(_), _) => "to_do",
                    (None, FrameKind::Item { ordered: true }) => "numbered_list_item",
                    _ => "bulleted_list_item",
                };
                let mut body = json!({ "rich_text": f.text });
                if let Some(checked) = f.todo {
                    body["checked"] = Value::Bool(checked);
                }
                if !f.children.is_empty() {
                    body["children"] = Value::Array(f.children);
                }
                let mut block = json!({"object": "block", "type": key});
                block[key] = body;
                self.push_block(block);
            }
            TagEnd::TableCell => {
                let rich = std::mem::take(&mut self.rich);
                if let Some(t) = &mut self.table {
                    t.cells.push(Value::Array(rich));
                }
            }
            // The header emits its cells directly inside TableHead, so it
            // closes a row just like TableRow does.
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let mut cells = std::mem::take(&mut t.cells);
                    // Every row must have exactly `table_width` cells.
                    cells.resize(t.width, Value::Array(Vec::new()));
                    t.rows.push(json!({
                        "object": "block",
                        "type": "table_row",
                        "table_row": { "cells": cells },
                    }));
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    let mut header = true;
                    for chunk in t.rows.chunks(MAX_TABLE_ROWS) {
                        self.push_block(json!({
                            "object": "block",
                            "type": "table",
                            "table": {
                                "table_width": t.width,
                                "has_column_header": header,
                                "has_row_header": false,
                                "children": chunk,
                            },
                        }));
                        header = false;
                    }
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_rich();
                let f = match self.frames.pop() {
                    Some(f) => f,
                    None => return,
                };
                let mut body = json!({ "rich_text": f.text });
                if !f.children.is_empty() {
                    body["children"] = Value::Array(f.children);
                }
                let mut block = json!({"object": "block", "type": "quote"});
                block["quote"] = body;
                self.push_block(block);
            }
            TagEnd::Emphasis => self.styles.italic = false,
            TagEnd::Strong => self.styles.bold = false,
            TagEnd::Strikethrough => self.styles.strikethrough = false,
            TagEnd::Link => self.styles.link = None,
            TagEnd::Image => self.in_image = false,
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str, code: bool) {
        if text.is_empty() {
            return;
        }
        let mut st = self.styles.clone();
        if code {
            st.code = true;
        }
        for chunk in chunk_str(text, MAX_TEXT_LEN) {
            self.rich.push(make_span(chunk, &st));
        }
    }

    /// Move pending rich text to where it belongs: the enclosing container's
    /// own text if it has none yet, otherwise a paragraph block.
    fn flush_rich(&mut self) {
        let rich = std::mem::take(&mut self.rich);
        if rich.is_empty() {
            return;
        }
        if let Some(f) = self.frames.last_mut() {
            if f.text.is_empty() {
                f.text = rich;
                return;
            }
        }
        self.push_block(paragraph(rich));
    }

    fn push_block(&mut self, block: Value) {
        match self.frames.last_mut() {
            Some(f) => f.children.push(block),
            None => self.top.push(block),
        }
    }
}

/// Notion only accepts absolute URLs on rich text links.
fn is_linkable(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    ["http://", "https://", "mailto:", "tel:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme) && url.len() > scheme.len())
}

fn paragraph(rich: Vec<Value>) -> Value {
    json!({"object": "block", "type": "paragraph", "paragraph": { "rich_text": rich }})
}

fn make_span(text: &str, st: &Styles) -> Value {
    let mut txt = json!({ "content": text });
    if let Some(url) = &st.link {
        txt["link"] = json!({ "url": url });
    }
    let mut span = json!({ "type": "text", "text": txt });
    let mut ann = Map::new();
    if st.bold {
        ann.insert("bold".into(), Value::Bool(true));
    }
    if st.italic {
        ann.insert("italic".into(), Value::Bool(true));
    }
    if st.strikethrough {
        ann.insert("strikethrough".into(), Value::Bool(true));
    }
    if st.code {
        ann.insert("code".into(), Value::Bool(true));
    }
    if !ann.is_empty() {
        span["annotations"] = Value::Object(ann);
    }
    span
}

/// Split on char boundaries into pieces of at most `max` characters.
fn chunk_str(s: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut count = 0;
    for (i, _) in s.char_indices() {
        if count == max {
            out.push(&s[start..i]);
            start = i;
            count = 0;
        }
        count += 1;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Notion rejects code blocks whose `language` is not in its supported set,
/// so map common aliases and fall back to "plain text".
fn notion_language(lang: &str) -> &'static str {
    const SUPPORTED: &[&str] = &[
        "abap", "arduino", "bash", "basic", "c", "clojure", "coffeescript", "c++", "c#", "css",
        "dart", "diff", "docker", "elixir", "elm", "erlang", "flow", "fortran", "f#", "gherkin",
        "glsl", "go", "graphql", "groovy", "haskell", "html", "java", "javascript", "json",
        "julia", "kotlin", "latex", "less", "lisp", "livescript", "lua", "makefile", "markdown",
        "markup", "matlab", "mermaid", "nix", "objective-c", "ocaml", "pascal", "perl", "php",
        "plain text", "powershell", "prolog", "protobuf", "python", "r", "reason", "ruby", "rust",
        "sass", "scala", "scheme", "scss", "shell", "sql", "swift", "typescript", "vb.net",
        "verilog", "vhdl", "visual basic", "webassembly", "xml", "yaml",
    ];
    let l = lang.trim().to_lowercase();
    let l = l.split(|c: char| c.is_whitespace() || c == ',').next().unwrap_or("");
    let mapped = match l {
        "js" | "jsx" | "node" => "javascript",
        "ts" | "tsx" => "typescript",
        "sh" | "zsh" | "console" | "shell-session" => "shell",
        "py" | "python3" => "python",
        "rb" => "ruby",
        "yml" => "yaml",
        "cpp" | "cc" | "cxx" => "c++",
        "cs" | "csharp" => "c#",
        "fs" | "fsharp" => "f#",
        "objc" | "objectivec" => "objective-c",
        "golang" => "go",
        "dockerfile" => "docker",
        "make" | "mk" => "makefile",
        "md" => "markdown",
        "tex" => "latex",
        "ps1" | "psh" => "powershell",
        "" | "text" | "txt" | "plaintext" | "plain" => "plain text",
        other => other,
    };
    SUPPORTED
        .iter()
        .find(|s| **s == mapped)
        .copied()
        .unwrap_or("plain text")
}

/// Enforce Notion's two-level nesting limit per request: blocks deeper than
/// that are hoisted to sit after their parent at the deepest allowed level.
fn flatten_deep(blocks: Vec<Value>, depth: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for mut b in blocks {
        let type_key = b["type"].as_str().unwrap_or_default().to_string();
        // A table's rows are structural, not nested content: they stay
        // attached, and count as one level below the table (see below).
        if type_key == "table" {
            out.push(b);
            continue;
        }
        let kids = b[type_key.as_str()]["children"].take();
        if let Some(obj) = b[type_key.as_str()].as_object_mut() {
            obj.remove("children");
        }
        if let Value::Array(kids) = kids {
            let kids = flatten_deep(kids, depth + 1);
            if depth >= MAX_NESTING {
                out.push(b);
                out.extend(kids);
            } else {
                // A table kid at depth+1 carries its rows at depth+2; hoist
                // it next to its parent when that would exceed the limit.
                let (nested, hoisted): (Vec<_>, Vec<_>) = kids
                    .into_iter()
                    .partition(|k| k["type"] != "table" || depth + 2 <= MAX_NESTING);
                if !nested.is_empty() {
                    b[type_key.as_str()]["children"] = Value::Array(nested);
                }
                out.push(b);
                out.extend(hoisted);
            }
        } else {
            out.push(b);
        }
    }
    out
}

/// If the document starts with an H1, pull it out to use as the page title.
fn extract_title(blocks: &mut Vec<Value>) -> Option<String> {
    let first = blocks.first()?;
    if first["type"] != "heading_1" {
        return None;
    }
    let title = plain_text(&first["heading_1"]["rich_text"]);
    blocks.remove(0);
    let title = title.trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

fn plain_text(rich: &Value) -> String {
    rich.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s["text"]["content"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_comes_from_leading_h1() {
        let c = convert("# Hello World\n\nBody text.\n");
        assert_eq!(c.title.as_deref(), Some("Hello World"));
        assert_eq!(c.blocks.len(), 1);
        assert_eq!(c.blocks[0]["type"], "paragraph");
        assert_eq!(
            c.blocks[0]["paragraph"]["rich_text"][0]["text"]["content"],
            "Body text."
        );
    }

    #[test]
    fn no_title_without_leading_h1() {
        let c = convert("Just a paragraph.\n\n# Later Heading\n");
        assert_eq!(c.title, None);
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[1]["type"], "heading_1");
    }

    #[test]
    fn heading_levels_cap_at_three() {
        let c = convert("## Two\n\n#### Four\n");
        assert_eq!(c.blocks[0]["type"], "heading_2");
        assert_eq!(c.blocks[1]["type"], "heading_3");
    }

    #[test]
    fn inline_styles_and_links() {
        let c = convert("**bold** and *italic* and `code` and [link](https://example.com)\n");
        let rich = c.blocks[0]["paragraph"]["rich_text"].as_array().unwrap();
        assert_eq!(rich[0]["annotations"]["bold"], true);
        assert_eq!(rich[2]["annotations"]["italic"], true);
        assert_eq!(rich[4]["annotations"]["code"], true);
        let link = rich.last().unwrap();
        assert_eq!(link["text"]["link"]["url"], "https://example.com");
    }

    #[test]
    fn relative_and_anchor_links_become_plain_text() {
        let c = convert("[rel](./other.md) [anchor](#setup) [abs](https://example.com) [mail](mailto:a@b.c)\n");
        let rich = c.blocks[0]["paragraph"]["rich_text"].as_array().unwrap();
        let link_of = |i: usize| rich[i]["text"].get("link").cloned();
        assert_eq!(link_of(0), None);
        assert_eq!(link_of(2), None);
        assert_eq!(rich[4]["text"]["link"]["url"], "https://example.com");
        assert_eq!(rich[6]["text"]["link"]["url"], "mailto:a@b.c");
    }

    #[test]
    fn bulleted_and_numbered_lists() {
        let c = convert("- one\n- two\n\n1. first\n2. second\n");
        assert_eq!(c.blocks[0]["type"], "bulleted_list_item");
        assert_eq!(c.blocks[2]["type"], "numbered_list_item");
        assert_eq!(
            c.blocks[2]["numbered_list_item"]["rich_text"][0]["text"]["content"],
            "first"
        );
    }

    #[test]
    fn nested_lists_become_children() {
        let c = convert("- parent\n  - child\n");
        assert_eq!(c.blocks.len(), 1);
        let kids = &c.blocks[0]["bulleted_list_item"]["children"];
        assert_eq!(kids[0]["type"], "bulleted_list_item");
        assert_eq!(
            kids[0]["bulleted_list_item"]["rich_text"][0]["text"]["content"],
            "child"
        );
    }

    #[test]
    fn task_lists_become_todos() {
        let c = convert("- [x] done\n- [ ] pending\n");
        assert_eq!(c.blocks[0]["type"], "to_do");
        assert_eq!(c.blocks[0]["to_do"]["checked"], true);
        assert_eq!(c.blocks[1]["to_do"]["checked"], false);
    }

    #[test]
    fn code_blocks_keep_language() {
        let c = convert("```rust\nfn main() {}\n```\n");
        assert_eq!(c.blocks[0]["type"], "code");
        assert_eq!(c.blocks[0]["code"]["language"], "rust");
        assert_eq!(
            c.blocks[0]["code"]["rich_text"][0]["text"]["content"],
            "fn main() {}"
        );
    }

    #[test]
    fn language_aliases_map_and_unknowns_fall_back() {
        assert_eq!(notion_language("ts"), "typescript");
        assert_eq!(notion_language("Sh"), "shell");
        assert_eq!(notion_language("toml"), "plain text");
        assert_eq!(notion_language(""), "plain text");
    }

    #[test]
    fn quotes_and_dividers() {
        let c = convert("> quoted text\n\n---\n");
        assert_eq!(c.blocks[0]["type"], "quote");
        assert_eq!(
            c.blocks[0]["quote"]["rich_text"][0]["text"]["content"],
            "quoted text"
        );
        assert_eq!(c.blocks[1]["type"], "divider");
    }

    #[test]
    fn long_text_is_chunked() {
        let long = "x".repeat(4100);
        let c = convert(&long);
        let rich = c.blocks[0]["paragraph"]["rich_text"].as_array().unwrap();
        assert_eq!(rich.len(), 3);
        assert_eq!(rich[0]["text"]["content"].as_str().unwrap().len(), 2000);
        assert_eq!(rich[2]["text"]["content"].as_str().unwrap().len(), 100);
    }

    #[test]
    fn deep_nesting_is_flattened() {
        let md = "- a\n  - b\n    - c\n      - d\n        - e\n";
        let c = convert(md);
        fn max_depth(blocks: &[Value]) -> usize {
            blocks
                .iter()
                .map(|b| {
                    let t = b["type"].as_str().unwrap();
                    match b[t]["children"].as_array() {
                        Some(kids) => 1 + max_depth(kids),
                        None => 0,
                    }
                })
                .max()
                .unwrap_or(0)
        }
        assert!(max_depth(&c.blocks) <= 2);
        // No content lost in the flattening.
        fn count(blocks: &[Value]) -> usize {
            blocks
                .iter()
                .map(|b| {
                    let t = b["type"].as_str().unwrap();
                    1 + b[t]["children"].as_array().map(|k| count(k)).unwrap_or(0)
                })
                .sum()
        }
        assert_eq!(count(&c.blocks), 5);
    }

    #[test]
    fn tables_become_table_blocks() {
        let md = "| Name | *Role* |\n|---|---|\n| Ada | **lead** |\n| Grace | |\n";
        let c = convert(md);
        assert_eq!(c.blocks.len(), 1);
        let table = &c.blocks[0]["table"];
        assert_eq!(c.blocks[0]["type"], "table");
        assert_eq!(table["table_width"], 2);
        assert_eq!(table["has_column_header"], true);
        let rows = table["children"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["type"], "table_row");
        let cell = |r: usize, c: usize| rows[r]["table_row"]["cells"][c].clone();
        assert_eq!(cell(0, 0)[0]["text"]["content"], "Name");
        assert_eq!(cell(1, 0)[0]["text"]["content"], "Ada");
        assert_eq!(cell(1, 1)[0]["annotations"]["bold"], true);
        // Missing cells are padded so every row matches table_width.
        assert_eq!(cell(2, 1), json!([]));
    }

    #[test]
    fn deeply_nested_tables_are_hoisted() {
        let md = "- a\n  - b\n    | H |\n    |---|\n    | x |\n";
        let c = convert(md);
        // The table's rows count as a nesting level, so the table itself
        // must end up at depth <= 1.
        fn tables_ok(blocks: &[Value], depth: usize) -> bool {
            blocks.iter().all(|b| {
                let t = b["type"].as_str().unwrap();
                if t == "table" {
                    return depth + 1 <= MAX_NESTING
                        && b["table"]["children"].as_array().is_some();
                }
                b[t]["children"]
                    .as_array()
                    .map(|k| tables_ok(k, depth + 1))
                    .unwrap_or(true)
            })
        }
        assert!(tables_ok(&c.blocks, 0));
        // The table itself survived somewhere in the tree.
        fn count_tables(blocks: &[Value]) -> usize {
            blocks
                .iter()
                .map(|b| {
                    let t = b["type"].as_str().unwrap();
                    if t == "table" {
                        return 1;
                    }
                    b[t]["children"].as_array().map(|k| count_tables(k)).unwrap_or(0)
                })
                .sum()
        }
        assert_eq!(count_tables(&c.blocks), 1);
    }

    #[test]
    fn long_tables_split_at_row_limit() {
        let mut md = String::from("| N |\n|---|\n");
        for i in 0..150 {
            md.push_str(&format!("| {i} |\n"));
        }
        let c = convert(&md);
        assert_eq!(c.blocks.len(), 2);
        assert_eq!(c.blocks[0]["table"]["children"].as_array().unwrap().len(), 100);
        assert_eq!(c.blocks[1]["table"]["children"].as_array().unwrap().len(), 51);
        assert_eq!(c.blocks[0]["table"]["has_column_header"], true);
        assert_eq!(c.blocks[1]["table"]["has_column_header"], false);
    }

    #[test]
    fn external_images_become_image_blocks() {
        let c = convert("![alt text](https://example.com/pic.png)\n");
        assert_eq!(c.blocks[0]["type"], "image");
        assert_eq!(
            c.blocks[0]["image"]["external"]["url"],
            "https://example.com/pic.png"
        );
    }
}
