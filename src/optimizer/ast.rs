#[derive(Debug, PartialEq)]
pub enum AstNode {
    Text(String),
    CodeBlock {
        language: Option<String>,
        content: String,
    },
    Json(serde_json::Value),
}

/// A very simple AST parser that extracts markdown code blocks (```) and tries to parse JSON.
pub fn parse(input: &str) -> Vec<AstNode> {
    let mut nodes = Vec::new();
    let mut current_text = String::new();
    let mut in_code_block = false;
    let mut code_language: Option<String> = None;
    let mut code_content = String::new();

    for line in input.lines() {
        if line.starts_with("```") {
            if in_code_block {
                in_code_block = false;

                // Try parsing JSON if language is json or absent
                let parsed_json =
                    if code_language.as_deref() == Some("json") || code_language.is_none() {
                        serde_json::from_str(&code_content).ok()
                    } else {
                        None
                    };

                if let Some(json) = parsed_json {
                    nodes.push(AstNode::Json(json));
                } else {
                    nodes.push(AstNode::CodeBlock {
                        language: code_language.take(),
                        content: std::mem::take(&mut code_content),
                    });
                }
            } else {
                in_code_block = true;
                let lang = line[3..].trim();
                if !lang.is_empty() {
                    code_language = Some(lang.to_string());
                } else {
                    code_language = None;
                }

                if !current_text.is_empty() {
                    nodes.push(AstNode::Text(std::mem::take(&mut current_text)));
                }
            }
        } else {
            if in_code_block {
                code_content.push_str(line);
                code_content.push('\n');
            } else {
                current_text.push_str(line);
                current_text.push('\n');
            }
        }
    }

    if !current_text.is_empty() {
        nodes.push(AstNode::Text(current_text));
    }

    if in_code_block && !code_content.is_empty() {
        // Unclosed code block
        nodes.push(AstNode::CodeBlock {
            language: code_language,
            content: code_content,
        });
    }

    nodes
}
