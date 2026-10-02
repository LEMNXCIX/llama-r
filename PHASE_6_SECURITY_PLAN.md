# Fase 6 — Seguridad, Observabilidad y Hardening

> **Documento de ejecución para un agente de IA implementador**  
> Fuente: [`ARCHITECTURE_PLAN.md`](./ARCHITECTURE_PLAN.md) § Fase 6  
> Audiencia: agente implementador Rust — seguir este plan **en orden**, sin saltarse pasos, sin "mejoras creativas" fuera de alcance.  
> Ubicación: raíz del repo (`PHASE_6_SECURITY_PLAN.md`)  
> Planes hermanos: [`PHASE_3_RIG_ENGINE_PLAN.md`](./PHASE_3_RIG_ENGINE_PLAN.md) · [`PHASE_4_RAG_PLAN.md`](./PHASE_4_RAG_PLAN.md) · [`PHASE_5_HISTORY_PLAN.md`](./PHASE_5_HISTORY_PLAN.md)

---

## 0. Cómo usar este plan (léelo completo antes de tocar código)

### 0.1 Rol del implementador

Eres un implementador Rust en el repo **llama-r**. Tu trabajo es completar la **Fase 6: Seguridad, Observabilidad y Hardening** para que:

1. Exista un **audit log** persistente de cada invocación de tool MCP (éxito, error, args redactados).
2. Los **secretos** (tokens, API keys) nunca aparezcan en TOMLs ni en logs; solo via variables de entorno.
3. Exista **rate limiting** por agente/proyecto configurable vía `tower` middleware.
4. Los **tracing spans** cubran `agent.run`, `mcp.call` y `rag.query` con atributos estructurados.
5. Las **métricas** incluyan tool success rate, latency p95 y rag hit rate, expuestas en `/api/metrics`.
6. Los **valores PII** (tokens, claves, respuestas de herramientas marcadas como sensibles) se redacten en logs de forma configurable.
7. Exista un **documento de threat model** y un checklist de review de manifiestos TOML.

### 0.2 Reglas obligatorias

1. **Trabaja solo en Fase 6.** No implementes CLI ecosistema (Fase 7) ni ninguna otra feature fuera del alcance descrito.
2. **No rompas ningún port existente** (`AgentEngine`, `RagStore`, `ConversationStore`, `McpClient`). Todos los cambios son aditivos o de capa middleware.
3. **Defense in depth:** el audit log y el re-check de scope en tool calls ya existen en `McpToolBridge::invoke_json`. **NO quitarlos** — solo añadir logging estructurado encima.
4. **Nunca loguees en claro:** args de tools, tokens, API keys, ni el contenido de `content` de resultados MCP si `log_tool_results = false` en el manifiesto del agente.
5. **Después de cada subtarea (6.x):** `cargo check` y, cuando haya tests, `cargo test --target-dir target-tests`.
6. **No edites** `target/`, `target-tests/`, logs, ni archivos generados.
7. Si la API de `tower` o `metrics` no coincide exactamente con los snippets, **adapta al crate real**, pero mantén el comportamiento observable descrito.
8. Commits: solo si el usuario lo pide. No hagas `git push`.
9. Comentarios de código en inglés. Mensajes de error en inglés (consistente con el repo).
10. El endpoint `/api/metrics` es **informacional** — no requiere autenticación en esta fase, pero debe documentarse como "no exponer en producción sin auth".
11. El rate limiter es **best-effort**: si la configuración está ausente, se aplica un default permisivo, no un error fatal.
12. El documento de threat model es un **archivo Markdown** en `docs/THREAT_MODEL.md`. No es código.

### 0.3 Criterio de salida (Definition of Done)

La fase está **completa** solo si **todas** estas condiciones se cumplen:

| # | Condición |
|---|-----------|
| D1 | Cada invocación de tool MCP emite un evento de audit log estructurado (`tracing::info!` con campos: `agent_id`, `server_id`, `tool_name`, `success`, `duration_ms`, args redactados si `log_tool_args = false`). |
| D2 | El `AgentObservabilityConfig` del agente (`log_tool_args`, `log_tool_results`) controla qué se incluye en el log. |
| D3 | La validación al cargar TOMLs rechaza cualquier campo que contenga `_token`, `_secret`, `_key`, `_password` con un valor literal (no referencia `${ENV_VAR}`). |
| D4 | El `McpServerConfig` no persiste `auth_env` como valor en logs — solo el nombre de la variable de entorno, nunca el valor resuelto. |
| D5 | Rate limiter configurable via `RATE_LIMIT_REQUESTS_PER_MINUTE` y `RATE_LIMIT_BURST` en `.env`; se aplica como `tower` layer en el router HTTP. |
| D6 | Tracing spans: `agent.run` incluye `agent_id`, `model`, `mcp_sources_count`, `rag_sources_count`; `mcp.call` incluye `server_id`, `tool_name`, `success`, `duration_ms`; `rag.query` incluye `source_id`, `hits`, `duration_ms`. |
| D7 | `AppObservability` ampliado con: `tool_calls_total`, `tool_calls_success`, `tool_calls_error`, `rag_queries_total`, `rag_hits_total`, `tool_latency_ms_total`. |
| D8 | Endpoint `GET /api/metrics` devuelve JSON con el snapshot extendido (D7). Gated con `X-Debug: true` por ahora. |
| D9 | Existe `src/core/redact.rs` con función `redact_value(key: &str, value: &str) -> String` que redacta valores de keys sensibles. |
| D10 | Existe `docs/THREAT_MODEL.md` con: modelo de amenazas, decisiones de seguridad (scope, deny-all, secrets en env), checklist de review de manifiestos, y recomendaciones de hardening de red. |
| D11 | Todos los tests existentes siguen pasando (no se rompen rutas ni contratos). |
| D12 | `cargo fmt`, `cargo check`, `cargo test --target-dir target-tests` pasan. |
| D13 | `ROADMAP.md` + `AGENTS.md` actualizados con estado Fase 6. |

### 0.4 Qué YA existe (no reimplementar desde cero)

| Pieza | Ruta | Estado |
|-------|------|--------|
| `AgentObservabilityConfig` (`trace`, `log_tool_args`, `log_tool_results`) | `src/domain/agent.rs` | ✅ listo |
| `AppObservability` básico (http_requests, chat, fallback, errors) | `src/api/observability.rs` | ✅ básico — **ampliar** |
| `McpToolBridge::invoke_json` con scope re-check | `src/adapters/rig_engine/tools.rs` | ✅ listo — **agregar audit log** |
| `ScopedMcpTool::invoke` con tracing info | `src/adapters/rig_engine/mod.rs` línea 35 | ✅ existe — **agregar span** |
| Tracing global con `tracing-subscriber` | `src/main.rs` | ✅ listo |
| Spans básicos en `AgentRuntime.chat` | `src/services/agent_runtime.rs` | ✅ básico — **ampliar campos** |
| `McpServerConfig` con `auth_env` (referencia, no valor) | `src/adapters/mcp/mod.rs` o `registry.rs` | ✅ listo |
| CORS layer en `build_router` | `src/runtime.rs` | ✅ listo — **agregar rate limit layer** |
| `AgentConfig` con `mcp_sources`, `tools_override` | `src/domain/agent.rs` | ✅ listo |
| `docs/` carpeta | `docs/` | ✅ existe |
| Redact helper | `src/core/` | ❌ **no existe** |
| Rate limit middleware | `src/runtime.rs` | ❌ **no existe** |
| Audit log en tool calls | `src/adapters/rig_engine/tools.rs` | ❌ solo re-check, sin audit event |
| Métricas extendidas (tool, rag) | `src/api/observability.rs` | ❌ **faltan contadores** |
| `GET /api/metrics` | `src/api/` | ❌ **no existe** |
| Validación de secrets en TOML | `src/services/validation.rs` | ❌ parcial |
| `docs/THREAT_MODEL.md` | `docs/` | ❌ **no existe** |

### 0.5 Flujo objetivo después de Fase 6

```text
POST /api/chat
  → RateLimitLayer (tower) → si excede: 429 Too Many Requests
  → chat_core
  → AgentRuntime.chat
       │  span: agent.run { agent_id, model, mcp_sources_count, rag_sources_count }
       │
       ├─ McpToolBridge::invoke_json
       │    ├─ re-check scope (ya existía)
       │    ├─ budget.try_consume (ya existía)
       │    ├─ span: mcp.call { server_id, tool_name }
       │    ├─ ScopedMcpTool::invoke → MCP Server
       │    └─ AUDIT LOG: { agent_id, tool, success, duration_ms, args? }
       │
       ├─ RagStore.query_scoped
       │    └─ span: rag.query { sources_count, hits, duration_ms }
       │
       └─ AppObservability.record_tool_call(success/error, duration_ms)
                         .record_rag_query(hits, duration_ms)

GET /api/metrics (X-Debug: true)
  → ObservabilitySnapshot extendido { tool_calls_total, tool_calls_success,
      tool_calls_error, rag_queries_total, rag_hits_total, avg_tool_latency_ms }
```

### 0.6 Orden de implementación (NO reordenar)

```text
6.0  Preparación: dependencias + módulos nuevos
6.1  Audit log de tool calls (ampliar McpToolBridge)
6.2  Redact helper (src/core/redact.rs)
6.3  Validación de secrets en TOML loader
6.4  Spans estructurados completos (agent.run, mcp.call, rag.query)
6.5  Métricas extendidas + GET /api/metrics
6.6  Rate limiter tower layer
6.7  Threat model doc (docs/THREAT_MODEL.md)
6.8  Documentación: ROADMAP.md + AGENTS.md
```

---

## 1. Dependencias

### 1.1 Cargo.toml

Agrega el crate de rate limiting (tower ya existe como transitive dep):

```toml
[dependencies]
# ... existentes ...
tower = { version = "0.5", features = ["limit", "buffer"] }
```

> **Verifica** con `cargo tree | grep tower` si ya está disponible. Si ya existe como dep directo de `tower-http`, confirma que tiene las features `limit` y `buffer`. Si no, agrégalas explícitamente.

No se necesita un crate externo de métricas — usaremos `AtomicU64` existente, ampliado, igual que `AppObservability`.

---

## 2. Audit log de tool calls (subtarea 6.1)

### 2.1 Qué modificar

Archivo: `src/adapters/rig_engine/tools.rs`  
Función: `McpToolBridge::invoke_json`

### 2.2 Audit event format

Cada invocación de tool emite un `tracing::info!` con los siguientes campos estructurados:

```
event = "tool_audit"
agent_id    = scope.agent_id
project_id  = scope.project_id (Option)
server_id   = self.inner.def.server_id
tool_name   = self.inner.def.name
success     = true | false
duration_ms = elapsed tiempo de la llamada MCP
args        = (solo si log_tool_args == true, redactado si false)
result_preview = (solo si log_tool_results == true, primeros 200 chars)
```

### 2.3 Problema: `McpToolBridge` no tiene acceso a `AgentObservabilityConfig`

Actualmente `McpToolBridge` solo tiene `inner: ScopedMcpTool` y `budget`. Necesita los flags de observabilidad del agente. Agrega:

```rust
pub struct McpToolBridge {
    pub inner: ScopedMcpTool,
    pub budget: Arc<Mutex<ToolCallBudget>>,
    pub log_tool_args: bool,         // ← nuevo
    pub log_tool_results: bool,      // ← nuevo
    pub agent_id: String,            // ← nuevo (para audit log)
    pub observability: Arc<AppObservability>,  // ← nuevo
}
```

Y actualiza `McpToolBridge::new` y todos los lugares donde se construye (busca con grep `McpToolBridge::new` en `src/adapters/rig_engine/builder.rs`).

### 2.4 Código de `invoke_json` ampliado

```rust
pub async fn invoke_json(&self, args: Value) -> Result<String, String> {
    // Defense in depth: re-check scope (ya existía — NO borrar)
    if !self.inner.scope.allows_tool(&self.inner.def.server_id, &self.inner.def.name) {
        let err = format!(
            "tool '{}/{}' denied by agent scope",
            self.inner.def.server_id, self.inner.def.name
        );
        tracing::warn!(
            event = "tool_audit",
            agent_id = %self.agent_id,
            server_id = %self.inner.def.server_id,
            tool_name = %self.inner.def.name,
            success = false,
            denied = true,
            "tool call denied by scope"
        );
        self.observability.record_tool_call(false, 0);
        return Err(err);
    }

    {
        let mut b = self.budget.lock().await;
        b.try_consume()?;
    }

    // Log args (redact if not allowed)
    let args_log = if self.log_tool_args {
        args.to_string()
    } else {
        "[redacted]".to_string()
    };

    let started = std::time::Instant::now();
    let result = self.inner.invoke(args).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    let success = result.is_ok();

    match &result {
        Ok(output) => {
            let result_preview = if self.log_tool_results {
                let s = output.chars().take(200).collect::<String>();
                if output.len() > 200 { format!("{}…", s) } else { s }
            } else {
                "[redacted]".to_string()
            };
            tracing::info!(
                event = "tool_audit",
                agent_id = %self.agent_id,
                server_id = %self.inner.def.server_id,
                tool_name = %self.inner.def.name,
                success = true,
                duration_ms = duration_ms,
                args = %args_log,
                result_preview = %result_preview,
                "tool call completed"
            );
        }
        Err(e) => {
            tracing::warn!(
                event = "tool_audit",
                agent_id = %self.agent_id,
                server_id = %self.inner.def.server_id,
                tool_name = %self.inner.def.name,
                success = false,
                duration_ms = duration_ms,
                args = %args_log,
                error = %e,
                "tool call failed"
            );
        }
    }

    self.observability.record_tool_call(success, duration_ms);
    result
}
```

### 2.5 Pasar `AppObservability` al engine

El `RigAgentEngine` necesita `observability` para construir `McpToolBridge`. Agrega el campo:

```rust
pub struct RigAgentEngine {
    pub ollama_url: String,
    pub mcp_registry: Arc<dyn McpServerRegistry>,
    pub rag: Option<Arc<dyn RagStore>>,
    pub observability: Arc<AppObservability>,  // ← nuevo
}
```

Y propágalo desde `build_agent_runtime` en `runtime.rs`:

```rust
let engine: Arc<dyn AgentEngine> = Arc::new(RigAgentEngine::new(
    config.ollama_url.clone(),
    mcp_registry,
    rag.clone(),
    state.observability.clone(),  // ← nuevo
));
```

> **Nota:** `build_agent_runtime` se llama antes de `build_app_state`. Construye `AppObservability` primero (ya es `Arc::new(AppObservability::new())`) y pásalo explícitamente a `build_agent_runtime`.

---

## 3. Redact helper (subtarea 6.2)

### 3.1 Crear `src/core/redact.rs`

```rust
//! PII and secret redaction utilities for logs and audit events.
//!
//! Redaction is applied to field names that indicate sensitive data.
//! Values are never stored or logged in clear text when the field name
//! matches a known sensitive pattern.

/// Sensitive field name suffixes that always trigger redaction.
const SENSITIVE_SUFFIXES: &[&str] = &[
    "_token", "_secret", "_key", "_password", "_passwd",
    "_api_key", "_auth", "_credential",
];

/// Sensitive exact field names.
const SENSITIVE_EXACT: &[&str] = &["authorization", "cookie", "set-cookie", "token", "password"];

/// Returns `true` if the field name indicates a sensitive value.
pub fn is_sensitive(field_name: &str) -> bool {
    let lower = field_name.to_ascii_lowercase();
    if SENSITIVE_EXACT.contains(&lower.as_str()) {
        return true;
    }
    SENSITIVE_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// Returns the value as-is if the field is not sensitive, or `"[REDACTED]"` if it is.
pub fn redact_value(field_name: &str, value: &str) -> &'static str {
    if is_sensitive(field_name) { "[REDACTED]" } else { value }
}

/// Redacts a JSON object in-place: any key matching sensitive patterns
/// has its value replaced with the string `"[REDACTED]"`.
pub fn redact_json(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = value {
        for (key, val) in map.iter_mut() {
            if is_sensitive(key) {
                *val = serde_json::Value::String("[REDACTED]".to_string());
            } else {
                redact_json(val); // recurse into nested objects
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sensitive_suffixes() {
        assert!(is_sensitive("auth_token"));
        assert!(is_sensitive("api_key"));
        assert!(is_sensitive("db_password"));
        assert!(is_sensitive("ACCESS_SECRET"));
        assert!(!is_sensitive("agent_id"));
        assert!(!is_sensitive("tool_name"));
    }

    #[test]
    fn redact_json_replaces_sensitive_values() {
        let mut v = serde_json::json!({
            "agent_id": "test",
            "auth_token": "super-secret-value",
            "nested": { "api_key": "another-secret", "ok": "visible" }
        });
        redact_json(&mut v);
        assert_eq!(v["agent_id"], "test");
        assert_eq!(v["auth_token"], "[REDACTED]");
        assert_eq!(v["nested"]["api_key"], "[REDACTED]");
        assert_eq!(v["nested"]["ok"], "visible");
    }
}
```

### 3.2 Registrar en `src/core/mod.rs`

```rust
pub mod paths;
pub mod hot_reload;
pub mod redact;  // ← nuevo
```

---

## 4. Validación de secrets en TOML (subtarea 6.3)

### 4.1 Qué validar

Al cargar un agente TOML (en `AgentRegistry` o en `AgentConfig` deserialization), verificar que ningún campo de nombre sensible contenga un valor literal (no-vacío y no referencia `${VAR}`).

Las rutas de carga son:
- `src/services/agent_registry.rs` → función que lee y parsea TOML
- `src/services/validation.rs` → ya existe, agregar ahí la lógica

### 4.2 Lógica de validación

En `src/services/validation.rs`, agrega:

```rust
use crate::core::redact::is_sensitive;

/// Validates that no TOML key matching a sensitive pattern contains a literal secret.
/// Allowed values: empty string, or strings matching `${VAR_NAME}` pattern.
pub fn validate_no_literal_secrets(raw_toml: &str) -> Result<(), String> {
    let value: toml::Value = toml::from_str(raw_toml)
        .map_err(|e| format!("TOML parse error: {e}"))?;
    check_toml_value(&value, "")
}

fn check_toml_value(value: &toml::Value, parent_key: &str) -> Result<(), String> {
    match value {
        toml::Value::Table(table) => {
            for (key, val) in table {
                let full_key = if parent_key.is_empty() {
                    key.clone()
                } else {
                    format!("{parent_key}.{key}")
                };
                if is_sensitive(key) {
                    if let toml::Value::String(s) = val {
                        if !s.is_empty() && !s.starts_with("${") {
                            return Err(format!(
                                "Security violation: field '{}' appears to contain a literal secret. \
                                 Use an environment variable reference instead (e.g. OLLAMA_URL from .env). \
                                 Never embed secrets in TOML agent files.",
                                full_key
                            ));
                        }
                    }
                }
                check_toml_value(val, &full_key)?;
            }
        }
        toml::Value::Array(arr) => {
            for item in arr {
                check_toml_value(item, parent_key)?;
            }
        }
        _ => {}
    }
    Ok(())
}
```

### 4.3 Llamar la validación en el loader

En `src/services/agent_registry.rs`, antes de parsear el TOML:

```rust
// Before: let config: AgentConfig = toml::from_str(&content)?;
// After:
crate::services::validation::validate_no_literal_secrets(&content)
    .map_err(|e| {
        tracing::warn!(path = %path.display(), error = %e, "Agent TOML security validation failed");
        e
    })?;
let config: AgentConfig = toml::from_str(&content)?;
```

> **Comportamiento:** si un agente falla la validación, **se descarta** ese agente (warning en logs) pero **no se detiene** la carga del resto de agentes. Igual que el comportamiento actual ante errores de parseo.

### 4.4 Tests de validación

En `src/services/validation.rs` o `tests/`:

```rust
#[test]
fn rejects_literal_token_in_toml() {
    let toml = r#"
    name = "test"
    system_prompt = "hello"
    auth_token = "sk-12345-plaintext"
    "#;
    assert!(validate_no_literal_secrets(toml).is_err());
}

#[test]
fn accepts_env_reference() {
    let toml = r#"
    name = "test"
    system_prompt = "hello"
    auth_token = "${MY_TOKEN}"
    "#;
    assert!(validate_no_literal_secrets(toml).is_ok());
}

#[test]
fn accepts_empty_sensitive_field() {
    let toml = r#"
    name = "test"
    system_prompt = "hello"
    auth_token = ""
    "#;
    assert!(validate_no_literal_secrets(toml).is_ok());
}
```

---

## 5. Spans estructurados (subtarea 6.4)

### 5.1 Span `agent.run`

Ya existe en `AgentRuntime::chat` (líneas ~76–82 en `src/services/agent_runtime.rs`). Ampliarlo con más atributos:

```rust
// Antes:
let span = tracing::info_span!(
    "agent.run",
    agent_id = %agent.qualified_id(),
    mcp_sources = scope.mcp_sources.len(),
    rag_sources = scope.rag_sources.len(),
);

// Después (añadir model, timeout, max_tool_calls):
let span = tracing::info_span!(
    "agent.run",
    agent_id       = %agent.qualified_id(),
    model          = %run_req.model,
    mcp_sources    = scope.mcp_sources.len(),
    rag_sources    = scope.rag_sources.len(),
    max_tool_calls = scope.max_tool_calls,
    timeout_secs   = scope.timeout_secs,
);
```

Y dentro del mismo método, emitir un evento al finalizar con el resultado:

```rust
let started = std::time::Instant::now();
let result = self.engine.run(run_req).await.map_err(AppError::Runtime)?;
let duration_ms = started.elapsed().as_millis() as u64;

tracing::info!(
    event = "agent.completed",
    agent_id = %agent.qualified_id(),
    tool_calls = result.tool_calls,
    iterations = result.iterations,
    duration_ms = duration_ms,
    "agent run completed"
);
```

### 5.2 Span `mcp.call`

En `ScopedMcpTool::invoke` dentro de `src/adapters/rig_engine/mod.rs`:

```rust
pub async fn invoke(&self, args: Value) -> Result<String, String> {
    if !self.scope.allows_tool(&self.def.server_id, &self.def.name) {
        return Err(format!(
            "tool '{}/{}' denied by agent scope",
            self.def.server_id, self.def.name
        ));
    }

    let span = tracing::info_span!(
        "mcp.call",
        server_id = %self.def.server_id,
        tool_name = %self.def.name,
    );
    let _guard = span.enter();

    // Log ya existente — solo upgradearlo a debug para no duplicar con el audit
    tracing::debug!(
        server_id = %self.def.server_id,
        tool = %self.def.name,
        "scoped MCP tool invoke"
    );

    // ... resto sin cambios (call_tool, is_error check) ...
}
```

> **Nota:** el audit log completo (con duration_ms, success, redacted args) se emite en `McpToolBridge::invoke_json` (subtarea 6.1). El span en `ScopedMcpTool::invoke` es para tracing distribuido (jerarquía de spans); no dupliques el audit event.

### 5.3 Span `rag.query`

En `FileRagStore::query_scoped` (o donde sea que `RagStore` ejecute la búsqueda de similitud), wrappea con:

```rust
let span = tracing::info_span!(
    "rag.query",
    sources_count = scope.rag_sources.len(),
);
let _guard = span.enter();

let started = std::time::Instant::now();
// ... lógica de búsqueda existente ...
let duration_ms = started.elapsed().as_millis() as u64;

tracing::debug!(
    event = "rag.query.completed",
    hits = results.len(),
    duration_ms = duration_ms,
    "RAG query completed"
);

// Registra en métricas (ver §6)
// observability.record_rag_query(results.len(), duration_ms);
```

> **Problema de acceso a `observability` desde `FileRagStore`:** `FileRagStore` no tiene referencia a `AppObservability`. Hay dos opciones:
> 1. Pasar `Arc<AppObservability>` al constructor de `FileRagStore` (limpio pero requiere cambios en el wiring).
> 2. Usar un singleton estático con `AtomicU64` independiente en `FileRagStore` (más simple, pero menos flexible).
>
> **Elige la opción 1** (pasar `Arc<AppObservability>`) para mantener consistencia con el patrón del engine.

Modifica `FileRagStore::new`:

```rust
pub fn new(
    base_dir: impl Into<PathBuf>,
    embeddings: Arc<dyn EmbeddingProvider>,
    observability: Arc<AppObservability>,  // ← nuevo
) -> Self { ... }
```

Y actualiza `build_rag` en `runtime.rs` para pasarlo.

---

## 6. Métricas extendidas + `/api/metrics` (subtarea 6.5)

### 6.1 Ampliar `AppObservability`

En `src/api/observability.rs`, agrega los nuevos contadores:

```rust
#[derive(Debug, Default)]
pub struct AppObservability {
    // --- existing ---
    http_requests:            AtomicU64,
    chat_requests:            AtomicU64,
    fallback_count:           AtomicU64,
    provider_errors:          AtomicU64,
    grpc_errors:              AtomicU64,
    mcp_errors:               AtomicU64,
    chat_latency_ms_total:    AtomicU64,
    completed_chat_requests:  AtomicU64,

    // --- new for Phase 6 ---
    tool_calls_total:         AtomicU64,
    tool_calls_success:       AtomicU64,
    tool_calls_error:         AtomicU64,
    tool_latency_ms_total:    AtomicU64,
    rag_queries_total:        AtomicU64,
    rag_hits_total:           AtomicU64,
    rag_latency_ms_total:     AtomicU64,
}
```

Agrega los métodos:

```rust
impl AppObservability {
    // ... existing methods unchanged ...

    /// Record a tool call result.
    pub fn record_tool_call(&self, success: bool, duration_ms: u64) {
        self.tool_calls_total.fetch_add(1, Ordering::Relaxed);
        self.tool_latency_ms_total.fetch_add(duration_ms, Ordering::Relaxed);
        if success {
            self.tool_calls_success.fetch_add(1, Ordering::Relaxed);
        } else {
            self.tool_calls_error.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a RAG query result.
    pub fn record_rag_query(&self, hits: usize, duration_ms: u64) {
        self.rag_queries_total.fetch_add(1, Ordering::Relaxed);
        self.rag_hits_total.fetch_add(hits as u64, Ordering::Relaxed);
        self.rag_latency_ms_total.fetch_add(duration_ms, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ObservabilitySnapshot {
        let completed = self.completed_chat_requests.load(Ordering::Relaxed);
        let total_latency = self.chat_latency_ms_total.load(Ordering::Relaxed);
        let tool_total = self.tool_calls_total.load(Ordering::Relaxed);
        let tool_latency = self.tool_latency_ms_total.load(Ordering::Relaxed);
        let rag_total = self.rag_queries_total.load(Ordering::Relaxed);
        let rag_latency = self.rag_latency_ms_total.load(Ordering::Relaxed);

        ObservabilitySnapshot {
            // --- existing ---
            http_requests:    self.http_requests.load(Ordering::Relaxed),
            chat_requests:    self.chat_requests.load(Ordering::Relaxed),
            fallback_count:   self.fallback_count.load(Ordering::Relaxed),
            provider_errors:  self.provider_errors.load(Ordering::Relaxed),
            grpc_errors:      self.grpc_errors.load(Ordering::Relaxed),
            mcp_errors:       self.mcp_errors.load(Ordering::Relaxed),
            chat_latency_ms_total: total_latency,
            avg_chat_latency_ms: if completed == 0 { 0 } else { total_latency / completed },

            // --- new ---
            tool_calls_total:     tool_total,
            tool_calls_success:   self.tool_calls_success.load(Ordering::Relaxed),
            tool_calls_error:     self.tool_calls_error.load(Ordering::Relaxed),
            tool_success_rate_pct: if tool_total == 0 { 100 }
                else { self.tool_calls_success.load(Ordering::Relaxed) * 100 / tool_total },
            avg_tool_latency_ms: if tool_total == 0 { 0 } else { tool_latency / tool_total },
            rag_queries_total:    rag_total,
            rag_hits_total:       self.rag_hits_total.load(Ordering::Relaxed),
            avg_rag_latency_ms:   if rag_total == 0 { 0 } else { rag_latency / rag_total },
        }
    }
}
```

Actualiza `ObservabilitySnapshot`:

```rust
#[derive(Debug, Serialize, ToSchema)]
pub struct ObservabilitySnapshot {
    // existing
    pub http_requests:          u64,
    pub chat_requests:          u64,
    pub fallback_count:         u64,
    pub provider_errors:        u64,
    pub grpc_errors:            u64,
    pub mcp_errors:             u64,
    pub chat_latency_ms_total:  u64,
    pub avg_chat_latency_ms:    u64,

    // new
    pub tool_calls_total:       u64,
    pub tool_calls_success:     u64,
    pub tool_calls_error:       u64,
    pub tool_success_rate_pct:  u64,
    pub avg_tool_latency_ms:    u64,
    pub rag_queries_total:      u64,
    pub rag_hits_total:         u64,
    pub avg_rag_latency_ms:     u64,
}
```

### 6.2 Endpoint `GET /api/metrics`

Crea `src/api/metrics_api.rs`:

```rust
//! `GET /api/metrics` — debug observability snapshot.
//! Gated by `X-Debug: true`. Do NOT expose without auth in production.

use crate::api::handlers::AppState;
use crate::error::AppError;
use axum::{extract::State, http::HeaderMap, response::IntoResponse, Json};
use std::sync::Arc;

pub async fn get_metrics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let ok = headers.get("x-debug").and_then(|v| v.to_str().ok()) == Some("true");
    if !ok {
        return Err(AppError::Validation(
            "Metrics endpoint requires header X-Debug: true".into(),
        ));
    }
    Ok(Json(state.observability.snapshot()))
}
```

Registra en `build_router` en `runtime.rs`:

```rust
.route("/api/metrics", get(get_metrics))
```

Y el import correspondiente.

---

## 7. Rate limiter tower layer (subtarea 6.6)

### 7.1 Variables de entorno

Agrega a `src/config.rs`:

```rust
pub struct Config {
    // ... existing ...
    /// Maximum requests per minute across all routes. 0 = disabled.
    pub rate_limit_rpm: u64,
    /// Burst capacity (requests that can be queued above RPM limit).
    pub rate_limit_burst: u32,
}

// In from_env():
let rate_limit_rpm = std::env::var("RATE_LIMIT_RPM")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(0); // 0 = disabled by default

let rate_limit_burst = std::env::var("RATE_LIMIT_BURST")
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(10);
```

Y en `.env.example`:

```dotenv
# Rate limiting (requests per minute, 0 = disabled)
RATE_LIMIT_RPM=0
RATE_LIMIT_BURST=10
```

### 7.2 Aplicar el layer en `build_router`

En `src/runtime.rs`, función `build_router`, después del CORS layer:

```rust
use tower::limit::RateLimitLayer;
use std::time::Duration;

pub fn build_router(state: Arc<AppState>, config: &Config) -> Router {
    let mut router = Router::new()
        // ... rutas existentes ...
        .layer(cors);

    // Apply rate limit if configured
    if config.rate_limit_rpm > 0 {
        let rate = config.rate_limit_rpm;
        let burst = config.rate_limit_burst as usize;
        // tower::limit::RateLimitLayer tokens per second + burst
        // RPM → RPS conversion: rate/60 (minimum 1)
        let rps = (rate / 60).max(1);
        router = router.layer(
            tower::ServiceBuilder::new()
                .layer(tower::limit::RateLimitLayer::new(rps, Duration::from_secs(1)))
                .into_inner(),
        );
        tracing::info!(rpm = rate, burst = burst, rps = rps, "Rate limiter enabled");
    } else {
        tracing::info!("Rate limiter disabled (RATE_LIMIT_RPM=0)");
    }

    router
}
```

> **Nota importante:** `tower::limit::RateLimitLayer` aplica a nivel global (todas las rutas comparten el límite). Para rate limit por agente/proyecto (más granular), sería necesario un middleware axum personalizado con un `DashMap<String, LeakyBucket>`. Eso está fuera de scope de Fase 6 — documenta esta limitación en el archivo.

Actualiza la firma de `build_router` para recibir `config: &Config` y actualiza la única llamada a `build_router` en `build_runtime`.

### 7.3 Respuesta 429

`tower::limit::RateLimitLayer` devuelve un error de tipo `BoxError`. Para convertirlo a `429 Too Many Requests` con el formato JSON de error del proyecto, agrega un handler de errores:

```rust
use tower_http::catch_panic::CatchPanicLayer;
use axum::response::Response;

// En build_router, después del RateLimitLayer:
router = router.layer(axum::middleware::map_response(handle_rate_limit_error));

async fn handle_rate_limit_error(response: Response) -> Response {
    if response.status() == axum::http::StatusCode::TOO_MANY_REQUESTS {
        let body = crate::error::ErrorBody {
            error: crate::error::ErrorInfo {
                code: "rate_limit_exceeded",
                message: "Too many requests. Please slow down.".to_string(),
            },
        };
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            axum::Json(body),
        ).into_response();
    }
    response
}
```

---

## 8. Threat Model (subtarea 6.7)

### 8.1 Crear `docs/THREAT_MODEL.md`

Este es el contenido completo del documento a crear:

```markdown
# Llama-R — Threat Model v1.0

> Versión: 1.0 · Fecha: [fecha actual]  
> Alcance: llama-r como gateway de agentes con MCP, RAG y historial.

## 1. Activos a proteger

| Activo | Clasificación | Riesgo si comprometido |
|--------|---------------|------------------------|
| Claves API / tokens de MCP servers | Secreto | Acceso no autorizado a servicios externos |
| Prompts de sistema (agent TOML) | Confidencial | Bypass de instrucciones, manipulación |
| Historial de conversaciones | Privado | Exposición de PII, datos de negocio |
| Vectores RAG (knowledge bases) | Privado | Exfiltración de conocimiento propietario |
| Identidad del agente (scope) | Integridad | Privilege escalation entre agentes |

## 2. Modelo de confianza

```text
Nivel de confianza:
  ALTA   → proceso llama-r (runtime, adapters)
  MEDIA  → manifiestos TOML (editables por operador)
  BAJA   → MCP servers externos (third-party)
  NULA   → requests HTTP externos (usuarios finales, Fudi UI)
```

## 3. Superficies de ataque

### 3.1 HTTP API (axum)
- **Sin autenticación**: todas las rutas son públicas por defecto. Mitigación: desplegar detrás de proxy con auth (nginx, Caddy, etc.).
- **Inyección de prompts via `messages`**: el usuario puede intentar sobreescribir el system prompt. Mitigación: el system prompt se antepone siempre (posición 0) y no es modificable por el usuario.
- **Agent header spoofing (`X-Agent`, `X-Project`)**: cualquier cliente puede solicitar cualquier agente. Mitigación: futura auth de API key por header.

### 3.2 Manifiestos TOML
- **Secrets en claro**: un operador descuidado puede poner tokens en el TOML. Mitigación: `validate_no_literal_secrets` en el loader — el agente se descarta con warning.
- **Scope escalation**: un TOML malicioso puede intentar acceder a `mcp_sources` no autorizados. Mitigación: scope se construye en `ScopeBuilder` desde el TOML + validación de IDs; un MCP server no registrado en `mcp-servers/*.toml` no puede ser usado.

### 3.3 Tool calls MCP
- **Tool exfiltration**: un MCP server malicioso puede devolver datos de otro agente. Mitigación: cada agente solo conecta a `mcp_sources` declarados; scope re-check en `McpToolBridge::invoke_json`.
- **Prompt injection via tool results**: un tool result puede contener instrucciones para el LLM. Mitigación: los tool results se inyectan como mensajes de rol `tool` (no `system`); el LLM debería tratar el contexto como datos, no como instrucciones.
- **Exfiltración via args**: args sensibles (tokens, passwords) en la llamada a una tool. Mitigación: `log_tool_args = false` por defecto; redact en audit log.

### 3.4 RAG
- **Exfiltración cross-agent**: un agente consulta RAG de otro agente. Mitigación: `query_scoped` filtra estrictamente por `rag_sources` del scope; `upsert_scoped` verifica `rag_write` policy.
- **Poisoning**: un actor malicioso inyecta datos en una collection RAG via `/api/rag/ingest`. Mitigación: el endpoint requiere `X-Debug: true` (header manualmente habilitado), y el scope write policy del agente se re-verifica.

### 3.5 SQLite History
- **Acceso al archivo**: acceso físico al `data/history.db` expone todo el historial. Mitigación: cifrado en reposo a nivel SO / volumen; fuera de scope de Fase 6.
- **Cross-agent history leak**: un handler lee historial del agente equivocado. Mitigación: `list_conversations` y `get_history` filtran siempre por `agent_qualified_id`.

## 4. Decisiones de seguridad del diseño

| Decisión | Rationale |
|----------|-----------|
| `tools_override = []` → deny-all | Safe default; el acceso a tools debe ser explícito |
| Secrets solo por env var | El proceso hereda secretos en runtime, nunca los persiste en disco versionado |
| `McpServerConfig.auth_env` = nombre de variable, no valor | El token se lee en runtime con `std::env::var`, nunca aparece en TOML |
| `log_tool_args = false` por defecto | Un tool arg puede contener datos sensibles del usuario; el operador opta-in explícitamente |
| Scope inmutable por request | Imposible que una tool llame a otra herramienta fuera del scope del agente que la invocó |
| `rag_write = none` por defecto | Un agente no puede contaminar colecciones RAG salvo que se lo otorgue explícitamente |

## 5. Checklist de review de manifiestos TOML

Antes de desplegar un nuevo agente en producción, verifica:

- [ ] `mcp_sources` contiene SOLO los servidores MCP necesarios (mínimo privilegio)
- [ ] `tools_override` lista las tools específicas (evitar `["*"]` en producción)
- [ ] `rag_sources` lista SOLO las collections que el agente necesita leer
- [ ] `rag_write = "none"` o `"own_memory_only"` (evitar `"listed"` salvo necesidad explícita)
- [ ] No hay ningún campo con sufijo `_token`, `_key`, `_secret`, `_password` con valor literal
- [ ] `log_tool_args = false` y `log_tool_results = false` (defaults — no cambiar sin revisar PII)
- [ ] `max_tool_calls` y `max_iterations` son razonables (no `999`)
- [ ] `timeout_secs` no supera los SLAs del sistema

## 6. Recomendaciones de hardening para producción

1. **Proxy con auth**: desplegar llama-r detrás de nginx/Caddy con autenticación básica o API key.
2. **Cifrado en reposo**: cifrar el volumen que contiene `data/history.db` y `data/lancedb/`.
3. **Network policy**: restringir qué hosts puede contactar llama-r (allowlist de MCP servers).
4. **No exponer `/api/metrics` ni `/api/rag/*` sin auth**: son endpoints de debug.
5. **Logs sin PII**: confirmar que `log_tool_args = false` y `log_tool_results = false` en todos los agentes.
6. **Rotación de tokens**: usar secretos de corta duración en `auth_env`; llama-r relee `std::env::var` en cada request (no cachea el valor en el struct).
7. **Actualizar dependencias regularmente**: `cargo audit` semanalmente para detectar CVEs.
```

---

## 9. Estructura de módulos final

Después de Fase 6:

```text
src/
├── adapters/
│   ├── rig_engine/
│   │   ├── mod.rs        ← span mcp.call + agregar observability al struct (modificar)
│   │   └── tools.rs      ← audit log completo + McpToolBridge con observability (modificar)
│   └── rag/
│       └── file_store.rs ← span rag.query + observability campo (modificar)
│
├── api/
│   ├── metrics_api.rs    ← GET /api/metrics (NUEVO)
│   ├── observability.rs  ← AppObservability ampliado (modificar)
│   └── handlers.rs       ← sin cambios (metrics_api se importa en runtime.rs)
│
├── core/
│   ├── mod.rs            ← registrar redact (modificar)
│   └── redact.rs         ← redact_value + redact_json (NUEVO)
│
├── services/
│   └── validation.rs     ← validate_no_literal_secrets (modificar)
│
├── config.rs             ← rate_limit_rpm, rate_limit_burst (modificar)
└── runtime.rs            ← build_router acepta Config, RateLimitLayer, metrics route (modificar)

docs/
└── THREAT_MODEL.md       ← documento de threat model (NUEVO)
```

---

## 10. Secuencia de validación paso a paso

Ejecuta después de cada subtarea:

```bash
# Después de 6.0 (deps):
cargo check

# Después de 6.1 (audit log):
cargo test --target-dir target-tests -- tools::tests
# Verifica en logs que tool_audit aparece en output de test

# Después de 6.2 (redact):
cargo test --target-dir target-tests -- core::redact

# Después de 6.3 (validation):
cargo test --target-dir target-tests -- services::validation

# Después de 6.4 (spans):
cargo check
# Manual: cargo run, enviar un chat con agente, buscar spans en logs:
#   grep "agent.run\|mcp.call\|rag.query" logs/llama-r.log

# Después de 6.5 (métricas):
cargo check
# Manual: curl -H "X-Debug: true" http://localhost:3000/api/metrics | jq .

# Después de 6.6 (rate limit):
cargo check
# Manual con RATE_LIMIT_RPM=2 y burst de requests rápidos → 429

# Después de 6.7 (threat model):
# Solo verificar que el archivo existe y es Markdown válido
ls docs/THREAT_MODEL.md

# Final:
cargo fmt
cargo check
cargo test --target-dir target-tests
```

---

## 11. Tests de Fase 6

### 11.1 Audit log (src/adapters/rig_engine/tools.rs)

Los tests existentes en `tools.rs` (`allowed_tool_invokes_mcp`, `denied_tool_returns_error`) ya cubren el scope re-check. Amplíalos:

```rust
#[tokio::test]
async fn denied_tool_does_not_consume_budget_and_logs_warning() {
    // Igual que el test existente `denied_tool_returns_error`
    // pero verifica adicionalmente que el budget no se consumió
    // (ya está en el test existente — solo añadir comentario sobre audit log)
}

#[tokio::test]
async fn allowed_tool_records_observability() {
    let obs = Arc::new(AppObservability::new());
    let (tool, _) = make_scoped_tool(true);
    let bridge = McpToolBridge {
        inner: tool,
        budget: Arc::new(Mutex::new(ToolCallBudget::new(5))),
        log_tool_args: false,
        log_tool_results: false,
        agent_id: "test-agent".to_string(),
        observability: obs.clone(),
    };
    let _ = bridge.invoke_json(serde_json::json!({})).await;
    let snap = obs.snapshot();
    assert_eq!(snap.tool_calls_total, 1);
    assert_eq!(snap.tool_calls_success, 1);
    assert_eq!(snap.tool_calls_error, 0);
}
```

### 11.2 Redact (src/core/redact.rs)

Ya incluidos en §3.1. No se necesitan tests adicionales.

### 11.3 Validación de secrets

Ya incluidos en §4.4.

### 11.4 Rate limit (integración)

```rust
#[tokio::test]
async fn rate_limit_returns_429_when_exceeded() {
    // Construye un router con RATE_LIMIT_RPM muy bajo (1 RPM)
    // y verifica que el segundo request retorna 429.
    // Usa tower::ServiceExt::oneshot para tests unitarios del router.
    // Marca este test con #[ignore] si es lento o flaky.
}
```

---

## 12. Resumen de archivos a crear / modificar

| Acción | Archivo |
|--------|---------|
| **CREAR** | `src/core/redact.rs` |
| **CREAR** | `src/api/metrics_api.rs` |
| **CREAR** | `docs/THREAT_MODEL.md` |
| **MODIFICAR** | `src/adapters/rig_engine/tools.rs` — audit log + McpToolBridge con observability |
| **MODIFICAR** | `src/adapters/rig_engine/mod.rs` — span mcp.call + observability en RigAgentEngine |
| **MODIFICAR** | `src/adapters/rag/file_store.rs` — span rag.query + observability campo |
| **MODIFICAR** | `src/api/observability.rs` — contadores extendidos + snapshot ampliado |
| **MODIFICAR** | `src/core/mod.rs` — registrar módulo redact |
| **MODIFICAR** | `src/services/validation.rs` — validate_no_literal_secrets |
| **MODIFICAR** | `src/services/agent_registry.rs` — llamar validate_no_literal_secrets en loader |
| **MODIFICAR** | `src/services/agent_runtime.rs` — ampliar span agent.run con model y límites |
| **MODIFICAR** | `src/config.rs` — rate_limit_rpm, rate_limit_burst |
| **MODIFICAR** | `src/runtime.rs` — build_router acepta Config, rate limit layer, GET /api/metrics |
| **MODIFICAR** | `Cargo.toml` — verificar features tower `limit`+`buffer` |
| **MODIFICAR** | `.env.example` — RATE_LIMIT_RPM, RATE_LIMIT_BURST |
| **MODIFICAR** | `ROADMAP.md` — estado Fase 6 |
| **MODIFICAR** | `AGENTS.md` — nuevos endpoints + variables de entorno |

---

## 13. Notas de diseño importantes

### 13.1 Por qué audit log vía `tracing::info!` y no una tabla separada

Una tabla de audit en SQLite sería más robusta, pero agrega complejidad y depende de la Fase 5 (SQLite). La aproximación de `tracing::info!` con `event = "tool_audit"` permite:
- Filtrar el audit log con `grep '"event":"tool_audit"'` en logs estructurados.
- Ser ingestado por cualquier sistema de log management (Loki, ELK, etc.) sin código adicional.
- Cero dependencias nuevas en Fase 6.

Si se requiere audit log persistente en BD en el futuro, se agrega como Fase 6.5 o Fase 7.x.

### 13.2 Por qué rate limit global y no por agente

Rate limit por agente requiere:
1. Extraer el agente del request antes de aplicar el limit.
2. Un `DashMap<String, RateLimiter>` compartido entre threads.
3. Manejo de limpieza (evitar memory leak con agentes que ya no existen).

Esto es significativamente más complejo y no es el foco de Fase 6. El rate limit global protege contra abusos de la API completa. El rate limit por agente puede ser Fase 7.

### 13.3 Redact vs Encrypt

`redact_value` **no cifra** — simplemente reemplaza el valor con `"[REDACTED]"` en el log. El secreto real viaja por la red (HTTPS entre llama-r y el MCP server) pero no queda en disco en logs. El cifrado en reposo es responsabilidad del operador (SO / volumen cifrado).

### 13.4 `validate_no_literal_secrets` no es un firewall

El validator detecta el patrón más obvio (campo con nombre sensible y valor literal). Un operador determinado puede evadir esto nombrando el campo diferente. La seguridad real viene de:
1. Que el proceso corre con mínimos privilegios.
2. Que `.env` no está versionado (`.gitignore`).
3. Que los secretos se rotan regularmente.

### 13.5 Actualización de `AGENTS.md` (subtarea 6.8)

Agrega en la sección **HTTP API Quick Reference**:

```text
GET /api/metrics          (requiere X-Debug: true)
```

Agrega en **Environment**:

```text
- `RATE_LIMIT_RPM`: requests por minuto permitidos (0 = sin límite, default)
- `RATE_LIMIT_BURST`: capacidad de burst del rate limiter (default: 10)
```

Cambia en `ROADMAP.md`:

```markdown
| 6 | Seguridad + observabilidad | P1 | ✅ completado |
```
