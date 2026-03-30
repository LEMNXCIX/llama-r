use crate::optimizer::ast::{parse, AstNode};

pub fn apply_rule(rule_name: &str, input: &str) -> String {
    let nodes = parse(input);
    let mut output = String::new();

    for node in nodes {
        match node {
            AstNode::Text(ref text) => {
                output.push_str(text);
            }
            AstNode::CodeBlock {
                ref language,
                ref content,
            } => {
                if rule_name == "compress_code" {
                    // Minification for common languages: remove empty lines and comments (//, #)
                    let compressed = content
                        .lines()
                        .filter(|l| {
                            let trimmed = l.trim();
                            !trimmed.is_empty() && !trimmed.starts_with("//") && !trimmed.starts_with("#")
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let lang = language.as_deref().unwrap_or("");
                    output.push_str(&format!("```{}\n{}\n```\n", lang, compressed));
                } else {
                    let lang = language.as_deref().unwrap_or("");
                    output.push_str(&format!("```{}\n{}\n```\n", lang, content));
                }
            }
            AstNode::Json(ref val) => {
                if rule_name == "minify_json" {
                    // Minify JSON
                    if let Ok(minified) = serde_json::to_string(val) {
                        output.push_str(&format!("```json\n{}\n```\n", minified));
                    } else {
                        output.push_str(&format!("```json\n{}\n```\n", val));
                    }
                } else {
                    // Keep pretty printed or original
                    if let Ok(pretty) = serde_json::to_string_pretty(val) {
                        output.push_str(&format!("```json\n{}\n```\n", pretty));
                    } else {
                        output.push_str(&format!("```json\n{}\n```\n", val));
                    }
                }
            }
        }
    }

    output
}
