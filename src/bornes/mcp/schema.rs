use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;

use super::{CompressionLevel, Options};

/// Lazy schema loading (specs.md §6.1). `tools/list` normally returns name +
/// full description (sometimes a whole paragraph) + full `inputSchema` for
/// EVERY tool — that's paid for on EVERY session, even if the model uses
/// just one tool or none at all. Here we return minimal wrappers and stash
/// the original in `schemas` for `get_tool_schema` to fetch on demand
/// (mod.rs intercepts that call locally, it never reaches the real server).
const MAX_DESC_CHARS: usize = 140;

#[cfg(test)]
pub fn transform_tools_list(msg: &Value, schemas: &Mutex<HashMap<String, Value>>) -> Value {
    transform_tools_list_with_options(
        msg,
        schemas,
        &Options {
            lazy_schemas: true,
            ..Options::default()
        },
    )
}

pub fn transform_tools_list_with_options(
    msg: &Value,
    schemas: &Mutex<HashMap<String, Value>>,
    options: &Options,
) -> Value {
    let Some(tools) = msg.pointer("/result/tools").and_then(Value::as_array) else {
        return msg.clone(); // fail-open (rule 3): unexpected format, pass through as-is
    };

    let filtering = !options.include_tools.is_empty() || !options.exclude_tools.is_empty();
    if !options.lazy_schemas && !filtering {
        return msg.clone();
    }

    let mut cache = schemas.lock().unwrap();
    let mut compact: Vec<Value> = Vec::with_capacity(tools.len() + 1);
    for tool in tools {
        if !tool_allowed(tool, options) {
            continue;
        }
        if let Some(name) = tool.get("name").and_then(Value::as_str) {
            cache.insert(name.to_string(), tool.clone());
        }
        if options.lazy_schemas {
            compact.push(compress_tool_for_listing(tool, options.compression));
        } else {
            compact.push(tool.clone());
        }
    }
    if options.lazy_schemas {
        compact.push(synthetic_get_tool_schema_tool());
    }
    drop(cache);

    let mut out = msg.clone();
    out["result"]["tools"] = Value::Array(compact);

    // Business rule 6, applied to the whole JSON-RPC message: with just one
    // tiny tool, the wrapper + the synthetic tool can cost more than the
    // original — in that case, return it untransformed.
    let orig_len = serde_json::to_string(msg)
        .map(|s| s.len())
        .unwrap_or(usize::MAX);
    let new_len = serde_json::to_string(&out)
        .map(|s| s.len())
        .unwrap_or(usize::MAX);
    if new_len < orig_len { out } else { msg.clone() }
}

fn tool_allowed(tool: &Value, options: &Options) -> bool {
    let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
    let included = options.include_tools.is_empty()
        || options
            .include_tools
            .iter()
            .any(|candidate| candidate == name);
    included
        && !options
            .exclude_tools
            .iter()
            .any(|candidate| candidate == name)
}

fn compress_tool_for_listing(tool: &Value, level: CompressionLevel) -> Value {
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let short = match level {
        CompressionLevel::Low => description.to_string(),
        CompressionLevel::Medium => first_sentence(description),
        CompressionLevel::High | CompressionLevel::Max => String::new(),
    };
    json!({
        "name": name,
        "description": format!("{short} [full schema: get_tool_schema(\"{name}\")]"),
        "inputSchema": {"type": "object"},
    })
}

fn first_sentence(desc: &str) -> String {
    let cut = desc.find(". ").map(|i| i + 1).unwrap_or(desc.len());
    let sentence = desc[..cut.min(desc.len())].trim();
    if sentence.chars().count() > MAX_DESC_CHARS {
        format!(
            "{}…",
            sentence.chars().take(MAX_DESC_CHARS).collect::<String>()
        )
    } else {
        sentence.to_string()
    }
}

fn synthetic_get_tool_schema_tool() -> Value {
    json!({
        "name": "get_tool_schema",
        "description": "Returns the full schema (inputSchema + complete description) for a tool by name, before actually calling it.",
        "inputSchema": {
            "type": "object",
            "properties": { "tool_name": {"type": "string"} },
            "required": ["tool_name"],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool with a verbose description/schema, the size commonly seen in
    /// real MCP servers (specs §6.1) — a catalog with several of these is
    /// exactly the case that motivates lazy loading (a single tiny tool
    /// doesn't amortize the fixed cost of the synthetic `get_tool_schema`
    /// tool, see `tiny_single_tool_falls_back_to_original`).
    fn verbose_tool(name: &str) -> Value {
        json!({
            "name": name,
            "description": format!(
                "{name} searches the internal documentation corpus for relevant passages. \
                 Supports pagination, relevance scoring, optional filtering by document \
                 category, and returns highlighted excerpts around the matched terms."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Free-text search query"},
                    "category": {"type": "string", "enum": ["api", "runbook", "product", "faq"]},
                    "limit": {"type": "integer", "default": 10},
                },
                "required": ["query"],
            }
        })
    }

    #[test]
    fn transforms_tool_list_and_caches_full_schema() {
        let schemas = Mutex::new(HashMap::new());
        let tools_in = vec![verbose_tool("search_docs"), verbose_tool("search_runbooks")];
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "tools": tools_in }
        });

        let out = transform_tools_list(&msg, &schemas);
        let tools = out["result"]["tools"].as_array().unwrap();

        assert_eq!(tools.len(), 3); // 2 real tools (compressed) + synthetic get_tool_schema
        assert_eq!(tools[0]["name"], "search_docs");
        assert_eq!(tools[0]["inputSchema"], json!({"type": "object"}));
        let original_desc_len = tools_in[0]["description"].as_str().unwrap().len();
        assert!(tools[0]["description"].as_str().unwrap().len() < original_desc_len);
        assert_eq!(tools[2]["name"], "get_tool_schema");

        let cached = schemas.lock().unwrap();
        let full = cached.get("search_docs").unwrap();
        assert!(
            full["description"]
                .as_str()
                .unwrap()
                .contains("highlighted excerpts")
        );
        assert_eq!(full["inputSchema"]["required"], json!(["query"]));
    }

    #[test]
    fn tiny_single_tool_falls_back_to_original() {
        let schemas = Mutex::new(HashMap::new());
        let msg = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [{"name": "x", "description": "y", "inputSchema": {}}]}
        });
        let out = transform_tools_list(&msg, &schemas);
        // rule 6: the wrapper + synthetic tool would cost more than the tiny original
        assert_eq!(out, msg);
    }

    #[test]
    fn filters_tools_before_compression() {
        let schemas = Mutex::new(HashMap::new());
        let msg = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [verbose_tool("keep"), verbose_tool("drop")]}
        });
        let options = Options {
            lazy_schemas: true,
            include_tools: vec!["keep".to_string()],
            ..Options::default()
        };
        let out = transform_tools_list_with_options(&msg, &schemas, &options);
        let tools = out["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "keep");
        assert_eq!(tools[1]["name"], "get_tool_schema");
    }

    #[test]
    fn max_level_keeps_only_tool_names_in_the_listing() {
        let schemas = Mutex::new(HashMap::new());
        let msg = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [verbose_tool("search"), verbose_tool("update")]}
        });
        let options = Options {
            lazy_schemas: true,
            compression: CompressionLevel::Max,
            ..Options::default()
        };
        let out = transform_tools_list_with_options(&msg, &schemas, &options);
        assert_eq!(out["result"]["tools"][0]["name"], "search");
        assert_eq!(
            out["result"]["tools"][0]["description"],
            " [full schema: get_tool_schema(\"search\")]"
        );
        assert_eq!(
            out["result"]["tools"][0]["inputSchema"],
            json!({"type": "object"})
        );
    }
}
