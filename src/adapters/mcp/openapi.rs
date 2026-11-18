//! OpenAPI/Swagger connection: exposes each operation as an MCP tool.
//!
//! This is deliberately a small, useful subset: JSON specs (OpenAPI 3.x or Swagger 2.0),
//! `paths` with `get`/`post`/`put`/`patch`/`delete`, path/query/header parameters and an
//! optional JSON request body. It is not a validator. An unusable spec is an error naming
//! its path so boot can warn and skip the connection instead of failing.

use super::http::{apply_connection_headers, ResolvedAuth};
use crate::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
use async_trait::async_trait;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct OpenApiDoc {
    #[serde(default)]
    servers: Vec<Server>,
    /// Swagger 2.0 location, used when `servers` is empty.
    #[serde(default)]
    host: Option<String>,
    #[serde(rename = "basePath", default)]
    base_path: Option<String>,
    #[serde(default)]
    schemes: Vec<String>,
    #[serde(default)]
    paths: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct Server {
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Parameter {
    name: String,
    #[serde(rename = "in", default)]
    location: String,
    #[serde(default)]
    required: bool,
}

#[derive(Debug, Clone)]
struct OperationDef {
    method: Method,
    path: String,
    parameters: Vec<Parameter>,
    has_body: bool,
}

#[derive(Debug)]
pub struct OpenApiConnection {
    server_id: String,
    base_url: String,
    http: reqwest::Client,
    auth: ResolvedAuth,
    headers: Vec<(String, String)>,
    timeout: Duration,
    tools: Vec<McpToolDef>,
    operations: HashMap<String, OperationDef>,
}

impl OpenApiConnection {
    /// Parse a JSON spec file and derive the tool catalog. Errors always name `spec_path`.
    pub fn from_spec_path(
        server_id: impl Into<String>,
        spec_path: impl Into<String>,
        auth: ResolvedAuth,
        headers: Vec<(String, String)>,
        timeout: Duration,
    ) -> Result<Self, String> {
        let server_id = server_id.into();
        let spec_path = spec_path.into();
        let content = std::fs::read_to_string(&spec_path)
            .map_err(|err| format!("cannot read OpenAPI spec '{spec_path}': {err}"))?;
        let value: Value = serde_json::from_str(&content)
            .map_err(|err| format!("invalid JSON in OpenAPI spec '{spec_path}': {err}"))?;

        let supported_version = value
            .get("openapi")
            .and_then(Value::as_str)
            .is_some_and(|v| v.starts_with("3."))
            || value
                .get("swagger")
                .and_then(Value::as_str)
                .is_some_and(|v| v.starts_with("2."));
        if !supported_version {
            return Err(format!(
                "unsupported or missing OpenAPI version in spec '{spec_path}'"
            ));
        }

        let doc: OpenApiDoc = serde_json::from_value(value)
            .map_err(|err| format!("invalid OpenAPI document '{spec_path}': {err}"))?;
        let base_url = resolve_base_url(&doc)
            .ok_or_else(|| format!("OpenAPI spec '{spec_path}' has no server URL"))?;
        if doc.paths.is_empty() {
            return Err(format!("OpenAPI spec '{spec_path}' has no paths"));
        }

        let (tools, operations) = build_catalog(&server_id, &doc);
        if tools.is_empty() {
            return Err(format!(
                "OpenAPI spec '{spec_path}' has no supported operations"
            ));
        }

        Ok(Self {
            server_id,
            base_url,
            http: reqwest::Client::new(),
            auth,
            headers,
            timeout,
            tools,
            operations,
        })
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }
}

fn resolve_base_url(doc: &OpenApiDoc) -> Option<String> {
    if let Some(server) = doc.servers.first() {
        return Some(server.url.trim_end_matches('/').to_string());
    }
    let host = doc.host.as_ref()?;
    let scheme = doc
        .schemes
        .first()
        .cloned()
        .unwrap_or_else(|| "https".to_string());
    let base = doc.base_path.as_deref().unwrap_or("");
    Some(
        format!("{scheme}://{host}{base}")
            .trim_end_matches('/')
            .to_string(),
    )
}

const METHODS: &[(&str, Method)] = &[
    ("get", Method::GET),
    ("post", Method::POST),
    ("put", Method::PUT),
    ("patch", Method::PATCH),
    ("delete", Method::DELETE),
];

fn build_catalog(
    server_id: &str,
    doc: &OpenApiDoc,
) -> (Vec<McpToolDef>, HashMap<String, OperationDef>) {
    let mut tools = Vec::new();
    let mut operations = HashMap::new();
    let mut used_names: HashMap<String, usize> = HashMap::new();

    for (path, item) in &doc.paths {
        let shared: Vec<Parameter> = item
            .get("parameters")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        for (method_name, method) in METHODS {
            let Some(operation) = item.get(*method_name) else {
                continue;
            };
            let mut parameters = shared.clone();
            if let Some(values) = operation.get("parameters").and_then(Value::as_array) {
                parameters.extend(
                    values
                        .iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok()),
                );
            }
            let has_body = operation.get("requestBody").is_some()
                || operation.get("body").is_some()
                || operation
                    .get("parameters")
                    .and_then(Value::as_array)
                    .is_some_and(|values| {
                        values
                            .iter()
                            .any(|v| v.get("in").and_then(Value::as_str) == Some("body"))
                    });

            let preferred = operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(sanitize_name)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| sanitize_name(&format!("{method_name}_{path}")));
            let name = unique_name(preferred, &mut used_names);

            let description = operation
                .get("summary")
                .and_then(Value::as_str)
                .or_else(|| operation.get("description").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();

            let mut properties = Map::new();
            let mut required = Vec::new();
            for parameter in &parameters {
                properties.insert(
                    parameter.name.clone(),
                    json!({ "type": "string", "description": parameter.location }),
                );
                if parameter.required {
                    required.push(Value::String(parameter.name.clone()));
                }
            }
            if has_body {
                properties.insert("body".to_string(), json!({ "type": "object" }));
            }
            let input_schema = json!({
                "type": "object",
                "properties": properties,
                "required": required,
            });

            tools.push(McpToolDef {
                server_id: server_id.to_string(),
                name: name.clone(),
                description,
                input_schema,
            });
            operations.insert(
                name,
                OperationDef {
                    method: method.clone(),
                    path: path.clone(),
                    parameters,
                    has_body,
                },
            );
        }
    }
    (tools, operations)
}

fn unique_name(preferred: String, used: &mut HashMap<String, usize>) -> String {
    let count = used.entry(preferred.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        preferred
    } else {
        format!("{preferred}_{}", *count)
    }
}

fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_underscore = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn encode_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[async_trait]
impl McpClient for OpenApiConnection {
    async fn list_tools(&self, server_id: &str) -> Result<Vec<McpToolDef>, String> {
        if server_id != self.server_id {
            return Err(format!(
                "client bound to '{}', requested '{}'",
                self.server_id, server_id
            ));
        }
        Ok(self.tools.clone())
    }

    async fn call_tool(&self, req: McpCallRequest) -> Result<McpCallResult, String> {
        if req.server_id != self.server_id {
            return Err("server_id mismatch".to_string());
        }
        let op = self
            .operations
            .get(&req.tool_name)
            .ok_or_else(|| format!("unknown OpenAPI operation '{}'", req.tool_name))?;
        let args = req
            .arguments
            .as_object()
            .ok_or_else(|| "OpenAPI tool arguments must be a JSON object".to_string())?;

        let mut path = op.path.clone();
        for parameter in &op.parameters {
            if parameter.location != "path" {
                continue;
            }
            let value = args
                .get(&parameter.name)
                .and_then(scalar_to_string)
                .ok_or_else(|| format!("missing path parameter '{}'", parameter.name))?;
            path = path.replace(&format!("{{{}}}", parameter.name), &encode_segment(&value));
        }

        let mut body_keys = Vec::new();
        let mut query: Vec<(String, String)> = Vec::new();
        let mut header_params: Vec<(String, String)> = Vec::new();
        for parameter in &op.parameters {
            let Some(raw) = args.get(&parameter.name) else {
                continue;
            };
            body_keys.push(parameter.name.clone());
            let Some(value) = scalar_to_string(raw) else {
                continue;
            };
            match parameter.location.as_str() {
                "query" => query.push((parameter.name.clone(), value)),
                "header" | "cookie" => header_params.push((parameter.name.clone(), value)),
                _ => {}
            }
        }

        let mut url = format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        if !query.is_empty() {
            let encoded = query
                .iter()
                .map(|(key, value)| format!("{}={}", encode_segment(key), encode_segment(value)))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&encoded);
        }

        let mut builder = self
            .http
            .request(op.method.clone(), &url)
            .timeout(self.timeout);
        // Request-derived headers go on first; `apply_connection_headers` inserts the
        // connection's own headers/auth last so they cannot be spoofed.
        for (name, value) in header_params {
            builder = builder.header(name, value);
        }

        if op.has_body {
            let body = args.get("body").cloned().unwrap_or_else(|| {
                let mut map = Map::new();
                for (key, value) in args {
                    if key != "body" && !body_keys.contains(key) {
                        map.insert(key.clone(), value.clone());
                    }
                }
                Value::Object(map)
            });
            builder = builder.json(&body);
        }

        let mut request = builder.build().map_err(|err| err.to_string())?;
        apply_connection_headers(&mut request, &self.auth, &self.headers)?;

        let send_fut = self.http.execute(request);
        let response = if let Some(token) = req.cancel_token.as_ref() {
            tokio::select! {
                result = send_fut => result.map_err(|err| err.to_string())?,
                _ = token.cancelled() => {
                    return Err(format!("OpenAPI call '{}' cancelled", req.tool_name))
                }
            }
        } else {
            send_fut.await.map_err(|err| err.to_string())?
        };

        let status = response.status();
        let bytes = response.bytes().await.map_err(|err| err.to_string())?;
        let content = serde_json::from_slice::<Value>(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
        Ok(McpCallResult {
            content,
            is_error: !status.is_success(),
        })
    }

    async fn health(&self, server_id: &str) -> Result<(), String> {
        if server_id != self.server_id {
            return Err("server_id mismatch".to_string());
        }
        self.http
            .get(&self.base_url)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|err| err.to_string())?;
        Ok(())
    }
}
