use regex::Regex;
use std::sync::LazyLock;

static DATA_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\s+data-name="[^"]*""#).unwrap());

static CSS_VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"var\([^,)]+,\s*([^)]+)\)"#).unwrap());

pub fn is_design_context(tool: &str) -> bool {
    tool == "mcp__figma__get_design_context"
}

/// Trims a Figma `get_design_context` code block (the JSX/HTML reference
/// code that makes up ~97% of the payload). Two lossless-for-implementation
/// cuts:
///
///  1. `data-name` attributes — Figma layer names ("image",
///     "p.MuiTypography-root") that carry no implementation information.
///     `data-node-id` is kept (used to drill into child nodes).
///
///  2. CSS `var()` references resolved to their fallback value. The Figma
///     output wraps every design-token value in `var(--token, fallback)`;
///     the agent converts to the target project's design system anyway, so
///     only the concrete fallback matters. This alone removes ~7% because
///     the var names are long (`--font-family/font-2`, `--color/black/-87%`)
///     and repeat hundreds of times.
pub fn trim_code(code: &str) -> String {
    let without_names = DATA_NAME_RE.replace_all(code, "");
    CSS_VAR_RE.replace_all(&without_names, "$1").into_owned()
}

/// Trims every text block in a `get_design_context` content array in place.
/// Returns `true` if anything changed.
pub fn trim_content_blocks(blocks: &mut [serde_json::Value]) -> bool {
    let mut changed = false;
    for block in blocks.iter_mut() {
        if block.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        let Some(text) = block.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        if !text.contains("data-name=") && !text.contains("var(--") {
            continue;
        }
        let trimmed = trim_code(text);
        if trimmed.len() < text.len() {
            block["text"] = serde_json::Value::String(trimmed);
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_data_name_preserves_data_node_id() {
        let input = r#"<div data-node-id="1:2" data-name="Landing page" className="flex">"#;
        let out = trim_code(input);
        assert!(!out.contains("data-name"));
        assert!(out.contains(r#"data-node-id="1:2""#));
        assert!(out.contains(r#"className="flex""#));
    }

    #[test]
    fn resolves_css_var_to_fallback() {
        let input = r#"gap-[var(--item-spacing/32,32px)]"#;
        assert_eq!(trim_code(input), r#"gap-[32px]"#);
    }

    #[test]
    fn resolves_font_family_var() {
        let input = r#"font-[family-name:var(--font-family\/font-2,'Manrope:Bold')]"#;
        assert_eq!(trim_code(input), r#"font-[family-name:'Manrope:Bold']"#);
    }

    #[test]
    fn resolves_color_var() {
        let input = r#"text-[color:var(--color\/black\/-87\%,rgba(0,0,0,0.87))]"#;
        assert_eq!(trim_code(input), r#"text-[color:rgba(0,0,0,0.87)]"#);
    }

    #[test]
    fn var_without_fallback_is_untouched() {
        let input = r#"gap-[var(--spacing)]"#;
        assert_eq!(trim_code(input), input);
    }

    #[test]
    fn preserves_annotations_and_asset_urls() {
        let input = r#"<div data-annotation="tooltip" data-node-id="3:4"><img src="https://figma.com/asset/abc.png" /></div>"#;
        let out = trim_code(input);
        assert!(out.contains("data-annotation"));
        assert!(out.contains("https://figma.com/asset/abc.png"));
        assert!(out.contains("data-node-id"));
    }

    #[test]
    fn combined_savings_on_real_pattern() {
        let input = r#"<div data-node-id="7836:16140" data-name="Group" className="content-stretch flex flex-[1_0_0] gap-[var(--item-spacing\/32,32px)] items-start font-[family-name:var(--font-family\/font-2,'Manrope:Bold')] text-[color:var(--color\/black\/-87\%,rgba(0,0,0,0.87))]">"#;
        let out = trim_code(input);
        assert!(!out.contains("data-name"));
        assert!(out.contains("data-node-id"));
        assert!(out.contains("gap-[32px]"));
        assert!(out.contains("font-[family-name:'Manrope:Bold']"));
        assert!(out.contains("text-[color:rgba(0,0,0,0.87)]"));
        let saving = input.len() - out.len();
        assert!(saving > 0, "should save bytes: {saving}");
    }

    #[test]
    fn trim_content_blocks_modifies_matching_blocks() {
        let mut blocks = vec![
            serde_json::json!({"type": "text", "text": r#"<div data-name="X" className="flex">"#}),
            serde_json::json!({"type": "text", "text": "plain instruction text"}),
            serde_json::json!({"type": "image", "source": {"data": "base64..."}}),
        ];
        assert!(trim_content_blocks(&mut blocks));
        let t = blocks[0]["text"].as_str().unwrap();
        assert!(!t.contains("data-name"));
        // Instruction block untouched (no data-name, no var)
        assert_eq!(
            blocks[1]["text"].as_str().unwrap(),
            "plain instruction text"
        );
    }

    #[test]
    fn is_design_context_matches_correctly() {
        assert!(is_design_context("mcp__figma__get_design_context"));
        assert!(!is_design_context("mcp__figma__get_screenshot"));
        assert!(!is_design_context("mcp__figma__get_metadata"));
    }

    #[test]
    fn real_payload_saves_at_least_ten_percent() {
        let code = include_str!("../../../tests/fixtures/figma-design-context.txt");
        let trimmed = trim_code(code);
        let saving_pct = (code.len() - trimmed.len()) * 100 / code.len();
        assert!(
            saving_pct >= 10,
            "expected ≥10% saving, got {saving_pct}% ({} → {} chars)",
            code.len(),
            trimmed.len()
        );
    }

    #[test]
    fn real_payload_keeps_node_ids_and_classes() {
        let code = include_str!("../../../tests/fixtures/figma-design-context.txt");
        let trimmed = trim_code(code);
        assert!(!trimmed.contains("data-name="), "data-name should be gone");
        let orig_node_ids: Vec<&str> = regex::Regex::new(r#"data-node-id="[^"]*""#)
            .unwrap()
            .find_iter(code)
            .map(|m| m.as_str())
            .collect();
        for nid in &orig_node_ids {
            assert!(trimmed.contains(nid), "lost node id: {nid}");
        }
        let orig_classnames: Vec<&str> = regex::Regex::new(r#"className="[^"]*""#)
            .unwrap()
            .find_iter(code)
            .map(|m| m.as_str())
            .collect();
        // Same count of className attributes
        let trimmed_classnames: Vec<&str> = regex::Regex::new(r#"className="[^"]*""#)
            .unwrap()
            .find_iter(&trimmed)
            .map(|m| m.as_str())
            .collect();
        assert_eq!(
            orig_classnames.len(),
            trimmed_classnames.len(),
            "className count changed: {} → {}",
            orig_classnames.len(),
            trimmed_classnames.len()
        );
    }

    #[test]
    fn real_payload_keeps_asset_urls() {
        let code = include_str!("../../../tests/fixtures/figma-design-context.txt");
        let trimmed = trim_code(code);
        let urls: Vec<&str> = regex::Regex::new(r#"https://www\.figma\.com/api/mcp/asset/[^"]*"#)
            .unwrap()
            .find_iter(code)
            .map(|m| m.as_str())
            .collect();
        assert!(!urls.is_empty());
        for url in &urls {
            assert!(trimmed.contains(url), "lost asset URL: {url}");
        }
    }
}
