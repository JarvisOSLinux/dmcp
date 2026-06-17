//! Parse `@mcp.tool` decorator docstrings from Python source files.
//!
//! When an MCP server's manifest has no `description` field, this module
//! scans the server's install directory for Python files and extracts the
//! docstring of every function immediately decorated with `@mcp.tool` (or
//! `@server.tool`, `@app.tool`, etc. — any `@*.tool` variant).
//!
//! The first docstring found is used as the server-level description fallback.
//! All `(tool_name, docstring)` pairs are returned so callers can populate
//! per-tool descriptions in the search index as well.

use std::path::Path;

/// A single `@mcp.tool`-decorated function with its extracted docstring.
#[derive(Debug, Clone)]
pub struct ToolDoc {
    /// Python function name (used as the tool name).
    pub tool_name: String,
    /// Cleaned-up docstring text, or `None` if the function had no docstring.
    pub docstring: Option<String>,
}

/// Scan `dir` recursively for Python files and return all `@*.tool`-decorated
/// functions together with their docstrings.
///
/// Returns an empty `Vec` if the directory does not exist or contains no
/// Python files with `@*.tool` decorators.
pub fn extract_tool_docs(dir: &Path) -> Vec<ToolDoc> {
    let mut results = Vec::new();
    visit_dir(dir, &mut results);
    results
}

/// Return the first non-empty docstring found in `docs`, suitable for use as
/// a server-level description fallback.
pub fn first_description(docs: &[ToolDoc]) -> Option<String> {
    docs.iter()
        .find_map(|d| d.docstring.as_ref().filter(|s| !s.is_empty()).cloned())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn visit_dir(dir: &Path, out: &mut Vec<ToolDoc>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden dirs and common non-source dirs
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with('.') && name != "__pycache__" && name != "node_modules" {
                visit_dir(&path, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
            if let Ok(source) = std::fs::read_to_string(&path) {
                parse_python_tool_docs(&source, out);
            }
        }
    }
}

/// Parse a Python source string and collect `(fn_name, docstring)` for every
/// function that has an `@*.tool` decorator immediately before it.
fn parse_python_tool_docs(source: &str, out: &mut Vec<ToolDoc>) {
    let lines: Vec<&str> = source.lines().collect();
    let n = lines.len();
    let mut i = 0;

    while i < n {
        let trimmed = lines[i].trim();

        // Look for a line that is a `@*.tool` decorator
        if is_tool_decorator(trimmed) {
            // Collect consecutive decorator lines (there may be more than one)
            let mut j = i + 1;
            while j < n && lines[j].trim().starts_with('@') {
                j += 1;
            }

            // The next non-decorator line should be `def <name>(...)`
            if j < n {
                let def_line = lines[j].trim();
                if let Some(fn_name) = parse_def_name(def_line) {
                    // Look for a docstring on the very next non-empty line inside
                    // the function body (accounting for the `def` line itself).
                    let docstring = extract_docstring(&lines, j + 1);
                    out.push(ToolDoc {
                        tool_name: fn_name,
                        docstring,
                    });
                    i = j + 1;
                    continue;
                }
            }
        }

        i += 1;
    }
}

/// Returns `true` if `line` looks like `@*.tool` or `@*.tool(...)`.
fn is_tool_decorator(line: &str) -> bool {
    if !line.starts_with('@') {
        return false;
    }
    // Strip the leading `@` and any arguments `(...)`
    let body = line.trim_start_matches('@');
    let ident = body.split('(').next().unwrap_or(body).trim();
    // Accept `mcp.tool`, `server.tool`, `app.tool`, plain `tool`, etc.
    ident.ends_with(".tool") || ident == "tool"
}

/// Extract the function name from a `def <name>(...)` line.
fn parse_def_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("def ")?.trim_start();
    // Name is everything up to the first `(` or whitespace
    let name: String = rest
        .chars()
        .take_while(|&c| c != '(' && !c.is_whitespace())
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Try to read a Python docstring starting at line index `start`.
/// Handles triple-quoted strings (`"""..."""` and `'''...'''`).
/// Returns `None` if there is no docstring.
fn extract_docstring(lines: &[&str], start: usize) -> Option<String> {
    // Skip blank lines and find the first content line inside the function
    let mut i = start;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() {
        return None;
    }

    let first = lines[i].trim();

    // Determine the quote style
    let quote = if first.starts_with("\"\"\"") {
        "\"\"\""
    } else if first.starts_with("'''") {
        "'''"
    } else {
        return None;
    };

    let after_open = &first[quote.len()..];

    // Single-line docstring: `"""text"""`
    if let Some(end) = after_open.find(quote) {
        let text = after_open[..end].trim().to_string();
        return Some(text);
    }

    // Multi-line docstring: collect lines until closing `"""`
    let mut parts = vec![after_open.to_string()];
    i += 1;
    while i < lines.len() {
        let line = lines[i];
        if let Some(end) = line.find(quote) {
            let tail = line[..end].trim_end();
            if !tail.is_empty() {
                parts.push(tail.to_string());
            }
            break;
        }
        parts.push(line.trim_end().to_string());
        i += 1;
    }

    // Clean up: strip leading/trailing blank lines, then join
    let joined: String = parts
        .iter()
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();

    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn docs_from(src: &str) -> Vec<ToolDoc> {
        let mut out = Vec::new();
        parse_python_tool_docs(src, &mut out);
        out
    }

    #[test]
    fn single_line_docstring() {
        let src = r#"
@mcp.tool
def greet(name: str):
    """Say hello to the user."""
    return f"Hello, {name}!"
"#;
        let docs = docs_from(src);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].tool_name, "greet");
        assert_eq!(docs[0].docstring.as_deref(), Some("Say hello to the user."));
    }

    #[test]
    fn multi_line_docstring() {
        let src = r#"
@mcp.tool
def search(query: str):
    """
    Search the web for the given query.
    Returns a list of results.
    """
    pass
"#;
        let docs = docs_from(src);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].tool_name, "search");
        let d = docs[0].docstring.as_deref().unwrap_or("");
        assert!(d.contains("Search the web"), "got: {}", d);
        assert!(d.contains("Returns a list"), "got: {}", d);
    }

    #[test]
    fn no_docstring() {
        let src = r#"
@mcp.tool
def no_doc():
    pass
"#;
        let docs = docs_from(src);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].tool_name, "no_doc");
        assert!(docs[0].docstring.is_none());
    }

    #[test]
    fn server_tool_variant() {
        let src = r#"
@server.tool
def compute(x: int):
    """Compute something."""
    pass
"#;
        let docs = docs_from(src);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].tool_name, "compute");
    }

    #[test]
    fn multiple_tools() {
        let src = r#"
@mcp.tool
def tool_a():
    """Tool A."""
    pass

@mcp.tool
def tool_b():
    """Tool B."""
    pass
"#;
        let docs = docs_from(src);
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].tool_name, "tool_a");
        assert_eq!(docs[1].tool_name, "tool_b");
    }

    #[test]
    fn non_tool_decorator_ignored() {
        let src = r#"
@app.route("/")
def index():
    """Home page."""
    pass

@mcp.tool
def actual_tool():
    """Does something useful."""
    pass
"#;
        let docs = docs_from(src);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].tool_name, "actual_tool");
    }

    #[test]
    fn first_description_returns_first_docstring() {
        let docs = vec![
            ToolDoc {
                tool_name: "a".into(),
                docstring: None,
            },
            ToolDoc {
                tool_name: "b".into(),
                docstring: Some("B desc".into()),
            },
            ToolDoc {
                tool_name: "c".into(),
                docstring: Some("C desc".into()),
            },
        ];
        assert_eq!(first_description(&docs).as_deref(), Some("B desc"));
    }
}
