# Fase 7 — Extensibilidad del Ecosistema

> **Documento de ejecución para un agente de IA implementador**  
> Fuente: [`ARCHITECTURE_PLAN.md`](./ARCHITECTURE_PLAN.md) § Fase 7  
> Audiencia: agente implementador Rust — seguir este plan **en orden**, sin saltarse pasos, sin "mejoras creativas" fuera de alcance.  
> Ubicación: raíz del repo (`PHASE_7_ECOSYSTEM_PLAN.md`)  
> Planes hermanos: [`PHASE_3_RIG_ENGINE_PLAN.md`](./PHASE_3_RIG_ENGINE_PLAN.md) · [`PHASE_4_RAG_PLAN.md`](./PHASE_4_RAG_PLAN.md) · [`PHASE_5_HISTORY_PLAN.md`](./PHASE_5_HISTORY_PLAN.md) · [`PHASE_6_SECURITY_PLAN.md`](./PHASE_6_SECURITY_PLAN.md)

---

## 0. Cómo usar este plan (léelo completo antes de tocar código)

### 0.1 Rol del implementador

Eres un implementador Rust en el repo **llama-r**. Tu trabajo es completar la **Fase 7: Extensibilidad del ecosistema** para que:

1. El comando `init-agent` genere TOMLs v2 completos con scaffold de scope (mcp_sources, tools_override, rag_sources, memory, observability).
2. Exista el comando `mcp register` que valide conectividad a un MCP server antes de guardarlo.
3. Exista `docs/MCP_PLUGIN_CONTRACT.md` documentando el contrato que deben cumplir apps externas (como Fudi) para integrarse via MCP.
4. Exista un adapter `OpenAICompatibleProvider` que hable el formato `/v1/chat/completions` de OpenAI, permitiendo conectar a cualquier provider compatible (OpenAI, Anthropic-compatible, LM Studio, vLLM, etc.).
5. Exista `Dockerfile` + `docker-compose.yml` con volúmenes correctos para `data/`, `agents/`, `contextos/`, `mcp-servers/`, y `logs/`.
6. `AGENTS.md`, `README.md` y el formato `export-rules` estén totalmente actualizados con el estado final de la plataforma.

### 0.2 Reglas obligatorias

1. **Trabaja solo en Fase 7.** No reimplementes nada de Fases anteriores.
2. **El comando `init-agent` existente NO se rompe.** La nueva lógica es un superset: agrega secciones de scope que hoy no se generan. El comportamiento de `--full` (ver §2) es opcional; el default sigue siendo el template simple actual.
3. **El adapter `OpenAICompatibleProvider` implementa el trait `LLMProvider`** existente en `src/providers/mod.rs`. No modifiques el trait.
4. **El adapter OpenAI es feature-gated:** `openai-provider` (o `openai`). Sin la feature, el build compila igual que hoy. El wiring solo activa el provider si la feature está ON y la env var `OPENAI_API_KEY` (u otra) está presente.
5. **No pongas API keys en código.** Solo las lees de env vars.
6. **Docker no es obligatorio para el build normal.** El `Dockerfile` es standalone y no afecta `cargo build`.
7. **Después de cada subtarea (7.x):** `cargo check` y, cuando haya tests, `cargo test --target-dir target-tests`.
8. **No edites** `target/`, `target-tests/`, archivos generados ni logs.
9. Commits: solo si el usuario lo pide. No hagas `git push`.
10. Comentarios de código en inglés. Mensajes de error en inglés (consistente con el repo).
11. **`AGENTS.md` y `README.md` son la fuente de verdad** para operadores. Actualízalos al final (subtarea 7.6), una vez el código esté listo.

### 0.3 Criterio de salida (Definition of Done)

| # | Condición |
|---|-----------|
| D1 | `cargo run -- init-agent <name> --full` genera TOML v2 completo con secciones `[scope]`, `[memory]`, `[observability]` con comentarios inline. |
| D2 | `cargo run -- init-agent <name>` (sin `--full`) sigue generando el TOML simple actual sin romper nada. |
| D3 | `cargo run -- mcp register <id> --url <url>` valida conectividad al servidor (llama `tools/list`) y guarda `mcp-servers/<id>.toml`. |
| D4 | `cargo run -- mcp register <id> --command <cmd> [--args ...]` registra un server stdio y valida que el comando existe en PATH. |
| D5 | `cargo run -- mcp list` lista los servers registrados en `mcp-servers/*.toml` con estado enabled/disabled. |
| D6 | `cargo run -- mcp check <id>` valida conectividad de un server ya registrado. |
| D7 | Existe `docs/MCP_PLUGIN_CONTRACT.md` con el contrato completo para integrar apps externas via MCP. |
| D8 | Existe `src/providers/openai_compat.rs` con `OpenAICompatibleProvider` que implementa `LLMProvider`. |
| D9 | `OpenAICompatibleProvider` soporta: `/v1/chat/completions` (non-stream y stream SSE), `/v1/models`, health via `/v1/models`. |
| D10 | La feature Cargo `openai-provider` activa el adapter; sin ella, el build compila sin ese módulo. |
| D11 | Si `OPENAI_API_KEY` (o `LLM_API_KEY`) está presente al iniciar, llama-r detecta y puede usar `OpenAICompatibleProvider`. El operador puede forzarlo con `LLM_PROVIDER=openai`. |
| D12 | Existe `Dockerfile` multi-stage (build + runtime) que compila llama-r y expone el puerto `3000`. |
| D13 | Existe `docker-compose.yml` con servicio `llama-r` + volúmenes para `data/`, `agents/`, `contextos/`, `mcp-servers/`, `logs/`. |
| D14 | `README.md` actualizado con: nuevos comandos CLI, sección Docker, sección multi-provider, sección historial/RAG (Fases 5–6). |
| D15 | `AGENTS.md` actualizado con todos los comandos CLI nuevos y el estado final de todas las fases. |
| D16 | `export-rules` genera una sección adicional `## Llama-R Agent Manifest` al final de cada formato, con los campos clave del agente (model, mcp_sources, tools_override) para que el IDE AI conozca el contexto. |
| D17 | `cargo fmt`, `cargo check`, `cargo test --target-dir target-tests` pasan. |
| D18 | `ROADMAP.md` actualizado con estado Fase 7 (completado). |

### 0.4 Qué YA existe (no reimplementar desde cero)

| Pieza | Ruta | Estado |
|-------|------|--------|
| CLI `clap` con `Commands` enum | `src/cli/commands.rs` | ✅ listo — **extender** |
| `Commands::InitAgent { name }` | `src/cli/commands.rs` línea 18 | ✅ — agregar `--full` flag |
| `init_agent_file` helper | `src/cli/commands.rs` línea 157 | ✅ — extender para v2 |
| `validate_identifier` | `src/services/validation.rs` | ✅ — usar en `mcp register` |
| `McpServerConfig` struct | `src/adapters/mcp/registry.rs` | ✅ — serializar a TOML |
| `HttpMcpClient` con `list_tools` | `src/adapters/mcp/http.rs` | ✅ — usar en `mcp check` |
| Trait `LLMProvider` | `src/providers/mod.rs` | ✅ — implementar para OpenAI |
| `OllamaProvider` como referencia | `src/providers/ollama.rs` | ✅ — copiar patrón |
| `build_runtime` con provider DI | `src/runtime.rs` | ✅ — agregar lógica de selección de provider |
| `docs/` directorio | `docs/` | ✅ — crear `MCP_PLUGIN_CONTRACT.md` |
| `export-rules` implementado | `src/cli/commands.rs` línea 243 | ✅ — extender con sección manifest |
| `AgentConfig` completo v2 | `src/domain/agent.rs` | ✅ — usar para generar TOML |
| `.gitignore` | `.gitignore` | ✅ — verificar `data/` |
| Multi-provider CLI en `.env.example` | `.env.example` | ✅ parcial (comentado) — actualizar |
| `OpenAICompatibleProvider` | `src/providers/` | ❌ **no existe** |
| `Commands::Mcp { ... }` | `src/cli/commands.rs` | ❌ **no existe** |
| `--full` flag en `init-agent` | `src/cli/commands.rs` | ❌ **no existe** |
| `Dockerfile` | raíz del repo | ❌ **no existe** |
| `docker-compose.yml` | raíz del repo | ❌ **no existe** |
| `docs/MCP_PLUGIN_CONTRACT.md` | `docs/` | ❌ **no existe** |

### 0.5 Orden de implementación (NO reordenar)

```text
7.1  init-agent --full: TOML v2 con scope skeleton
7.2  mcp register / mcp list / mcp check (CLI)
7.3  Plugin contract: docs/MCP_PLUGIN_CONTRACT.md
7.4  OpenAICompatibleProvider adapter (feature-gated)
7.5  Dockerfile + docker-compose.yml
7.6  Documentación final: README.md + AGENTS.md + export-rules + ROADMAP.md
```

---

## 1. Subtarea 7.1 — `init-agent --full`: TOML v2 con scope skeleton

### 1.1 Modificar `Commands::InitAgent`

En `src/cli/commands.rs`, amplía el enum:

```rust
#[derive(Subcommand, Debug)]
pub enum Commands {
    // ... existing ...

    /// Initialize a new editable project agent configuration
    InitAgent {
        name: String,
        /// Generate a full v2 TOML with scope, memory and observability sections.
        #[arg(long, default_value = "false")]
        full: bool,
    },

    // ... rest ...
}
```

Actualiza el `match` en `handle_cli`:

```rust
Some(Commands::InitAgent { name, full }) => {
    init_named_agent(name, *full).await;
    true
}
```

### 1.2 Modificar `init_named_agent`

```rust
async fn init_named_agent(name: &str, full: bool) {
    if let Err(err) = crate::services::validation::validate_identifier(name, "agent name") {
        println!("{}", err);
        return;
    }

    let project_id = match current_project_id() {
        Ok(id) => id,
        Err(err) => { println!("{}", err); return; }
    };

    let context_dir = crate::core::paths::get_project_context_dir(&project_id);
    if !context_dir.exists() {
        println!("Warning: Project '{}' has not been analyzed yet.", project_id);
        println!("Hint: Run `llama-r analyze .` to generate context for this project.");
        println!();
    }

    let prompt = format!(
        "You are a specialized agent named '{}' for the project '{}'.",
        name, project_id
    );

    if full {
        init_agent_file_v2(name, &prompt, &project_id).await;
    } else {
        init_agent_file(name, name, &prompt, Some(&project_id), &[]).await;
    }
}
```

### 1.3 Crear `init_agent_file_v2`

```rust
async fn init_agent_file_v2(name: &str, system_prompt: &str, project_id: &str) {
    let agents_dir = crate::core::paths::get_project_agents_dir(project_id);
    let filename = format!("{}.toml", name);
    let path = agents_dir.join(&filename);

    if path.exists() {
        println!(
            "Agent config '{}' already exists in {}.",
            filename, agents_dir.display()
        );
        println!("You can edit it directly; the file is meant to stay editable.");
        return;
    }

    if let Err(err) = std::fs::create_dir_all(&agents_dir) {
        println!("Failed to create agents directory: {}", err);
        return;
    }

    let default_model = std::env::var("DEFAULT_MODEL").unwrap_or_default();
    let model_line = if default_model.is_empty() {
        "# model = \"\"  # Falls back to DEFAULT_MODEL from .env".to_string()
    } else {
        format!("model = \"{}\"", default_model)
    };

    // Generate a v2 TOML with full scope skeleton and inline comments
    let config = format!(
        r#"# Llama-R Agent Manifest v2
# Agent: {name} | Project: {project_id}
# Generated by: llama-r init-agent {name} --full
# Edit freely — hot reload picks up changes without restarting the server.

name = "{name}"
description = "Specialized agent '{name}' for project '{project_id}'."
{model_line}
system_prompt = """
{system_prompt}
"""
context_project = "{project_id}"
rules = []
skills = []
auto_skills = []

[optimize]
enabled = true
rules = []

# ── SCOPE (isolation boundary) ──────────────────────────────────────────────
# mcp_sources: MCP server ids this agent may discover tools from.
# Must match ids defined in mcp-servers/*.toml.
# Empty list = no MCP tools available (safe default).
mcp_sources = []

# tools_override: explicit tool allowlist.
# []      → deny-all (safe default when mcp_sources is empty)
# ["*"]   → all tools from mcp_sources (use with care)
# ["server/tool_name", ...] → strict allowlist
tools_override = []

# rag_sources: knowledge bases this agent may read.
# Namespaces: "project:<id>/collection", "agent:<project>/<agent>/memory", "global/<col>"
# Empty = no RAG access (safe default).
rag_sources = []

# rag_write: write policy for upserts.
# "none"            → no writes (safe default)
# "own_memory_only" → only agent:<project>/<agent>/memory
# "listed"          → any collection in rag_sources
rag_write = "none"

# Execution limits
max_tool_calls = 8
max_iterations = 6
timeout_secs   = 90
# temperature  = 0.7  # Uncomment to override model default

# ── MEMORY ──────────────────────────────────────────────────────────────────
[memory]
# Persist every user/assistant turn in SQLite history.db
persist_history = true

# Summarize and index every N turns into RAG memory (requires index_summaries = true)
summarize_every_n_turns = 10

# Enable indexing of summaries into the agent's RAG memory collection
index_summaries = false

# Target RAG collection for summaries (defaults to agent own-memory if unset)
# summary_collection = "agent:{project_id}/{name}/memory"

# Days to keep conversation history before automatic purge
retention_days = 90

# ── OBSERVABILITY ────────────────────────────────────────────────────────────
[observability]
trace = true
# Set to true only if tool arguments do NOT contain PII or secrets
log_tool_args   = false
log_tool_results = false
"#
    );

    match std::fs::write(&path, config) {
        Ok(_) => {
            println!("✅ Created full v2 agent config at '{}'", path.display());
            println!("\nTo use this agent, send these headers in your HTTP request:");
            println!("  X-Project: {}", project_id);
            println!("  X-Agent:   {}", name);
            println!("\nKey sections to edit:");
            println!("  mcp_sources    — add MCP server ids from mcp-servers/*.toml");
            println!("  tools_override — list allowed tools or use [\"*\"] for all");
            println!("  rag_sources    — add RAG collections this agent may read");
            if default_model.is_empty() {
                println!("\nNote: No DEFAULT_MODEL configured. Run `llama-r` first to set one.");
            }
        }
        Err(err) => println!("Error writing agent config: {}", err),
    }
}
```

### 1.4 Tests

```rust
#[test]
fn init_agent_v2_toml_is_valid() {
    // Genera el contenido del template v2 y verifica que es TOML válido
    // y que los campos de scope tienen defaults seguros.
    let content = /* render the same format string with test values */;
    let cfg: crate::domain::agent::AgentConfig = toml::from_str(&content).unwrap();
    assert!(cfg.mcp_sources.is_empty());
    assert!(cfg.tools_override.is_empty());
    assert!(cfg.rag_sources.is_empty());
    assert_eq!(cfg.rag_write, crate::domain::agent::RagWritePolicy::None);
    assert!(!cfg.memory.index_summaries);
    assert!(!cfg.observability.log_tool_args);
}
```

---

## 2. Subtarea 7.2 — CLI: `mcp register` / `mcp list` / `mcp check`

### 2.1 Nuevo subcomando `Mcp`

En `src/cli/commands.rs`, agrega un sub-subcommand:

```rust
#[derive(Subcommand, Debug)]
pub enum Commands {
    // ... existing ...

    /// Manage MCP server registrations
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum McpAction {
    /// Register a new MCP server and validate connectivity
    Register {
        /// Server id (used in mcp_sources of agent TOMLs)
        id: String,
        /// HTTP URL for http/sse transport (e.g. http://localhost:4100/mcp)
        #[arg(long)]
        url: Option<String>,
        /// Command for stdio transport (e.g. npx)
        #[arg(long)]
        command: Option<String>,
        /// Arguments for the stdio command
        #[arg(long, num_args = 0..)]
        args: Vec<String>,
        /// Tool namespace prefix (e.g. "fudi" → "fudi/tool_name")
        #[arg(long)]
        namespace: Option<String>,
        /// Environment variable holding the auth token (for http transport)
        #[arg(long)]
        auth_env: Option<String>,
        /// Timeout in seconds for HTTP requests
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// Register even if connectivity check fails
        #[arg(long, default_value = "false")]
        force: bool,
    },
    /// List all registered MCP servers
    List,
    /// Check connectivity to a registered MCP server
    Check {
        /// Server id to check
        id: String,
    },
    /// Remove a registered MCP server
    Remove {
        /// Server id to remove
        id: String,
    },
}
```

### 2.2 Handler en `handle_cli`

```rust
Some(Commands::Mcp { action }) => {
    handle_mcp_command(action).await;
    true
}
```

### 2.3 Implementación de `handle_mcp_command`

```rust
async fn handle_mcp_command(action: &McpAction) {
    match action {
        McpAction::Register { id, url, command, args, namespace, auth_env, timeout, force } => {
            mcp_register(id, url.as_deref(), command.as_deref(), args, namespace.as_deref(), auth_env.as_deref(), *timeout, *force).await;
        }
        McpAction::List => mcp_list().await,
        McpAction::Check { id } => mcp_check(id).await,
        McpAction::Remove { id } => mcp_remove(id).await,
    }
}
```

### 2.4 `mcp_register`

```rust
async fn mcp_register(
    id: &str,
    url: Option<&str>,
    command: Option<&str>,
    args: &[String],
    namespace: Option<&str>,
    auth_env: Option<&str>,
    timeout: u64,
    force: bool,
) {
    // 1. Validate id
    if let Err(err) = crate::services::validation::validate_identifier(id, "server id") {
        println!("Invalid server id: {}", err);
        return;
    }

    // 2. Determine transport and validate
    let transport = if url.is_some() { "http" } else if command.is_some() { "stdio" } else {
        println!("Error: specify --url (http) or --command (stdio)");
        return;
    };

    // 3. Connectivity check
    let check_ok = match transport {
        "http" => check_http_mcp(url.unwrap(), auth_env, timeout).await,
        "stdio" => check_stdio_command(command.unwrap()),
        _ => unreachable!(),
    };

    match &check_ok {
        Ok(tool_count) => println!("✅ Connectivity OK — discovered {} tools", tool_count),
        Err(err) => {
            println!("⚠️  Connectivity check failed: {}", err);
            if !force {
                println!("Use --force to register anyway.");
                return;
            }
            println!("Registering anyway (--force).");
        }
    }

    // 4. Build McpServerConfig and write TOML
    let config_toml = build_mcp_server_toml(id, transport, url, command, args, namespace, auth_env, timeout);
    let dir = crate::core::paths::get_base_dir().join("mcp-servers");
    if let Err(err) = std::fs::create_dir_all(&dir) {
        println!("Failed to create mcp-servers dir: {}", err);
        return;
    }
    let path = dir.join(format!("{}.toml", id));
    if path.exists() {
        println!("⚠️  '{}' already exists. Overwriting.", path.display());
    }
    match std::fs::write(&path, config_toml) {
        Ok(_) => {
            println!("✅ Saved: {}", path.display());
            println!("\nTo use this server in an agent, add to mcp_sources:");
            println!("  mcp_sources = [\"{}\"]", id);
            if let Some(ns) = namespace {
                println!("Tools will be prefixed as \"{ns}/<tool_name>\"");
            }
        }
        Err(err) => println!("Error writing config: {}", err),
    }
}
```

### 2.5 Helper: `check_http_mcp`

```rust
/// Calls tools/list on the MCP server. Returns tool count on success.
async fn check_http_mcp(url: &str, auth_env: Option<&str>, timeout_secs: u64) -> Result<usize, String> {
    let token = auth_env.and_then(|name| std::env::var(name).ok());
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {}
    });

    let mut req = client
        .post(url)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .header("Content-Type", "application/json")
        .json(&body);

    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }

    let resp = req.send().await.map_err(|e| format!("HTTP error: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Server returned HTTP {}", resp.status()));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| format!("JSON parse error: {e}"))?;
    if let Some(err) = json.get("error") {
        return Err(format!("MCP error: {err}"));
    }

    let tools = json
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    Ok(tools)
}
```

### 2.6 Helper: `check_stdio_command`

```rust
/// Verifies that the given command exists in PATH (no execution).
fn check_stdio_command(command: &str) -> Result<usize, String> {
    // Use `which` (unix) or check PATH manually
    let found = std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default()
    )
    .any(|dir| dir.join(command).exists());

    if found {
        Ok(0) // tool count unknown for stdio without spawning
    } else {
        Err(format!("command '{}' not found in PATH", command))
    }
}
```

### 2.7 Helper: `build_mcp_server_toml`

```rust
fn build_mcp_server_toml(
    id: &str,
    transport: &str,
    url: Option<&str>,
    command: Option<&str>,
    args: &[String],
    namespace: Option<&str>,
    auth_env: Option<&str>,
    timeout: u64,
) -> String {
    let mut lines = vec![
        format!("# MCP server: {id}"),
        format!("# Registered by: llama-r mcp register {id}"),
        String::new(),
        format!("id = \"{id}\""),
        format!("transport = \"{transport}\""),
    ];

    if let Some(u) = url {
        lines.push(format!("url = \"{u}\""));
    }
    if let Some(cmd) = command {
        lines.push(format!("command = \"{cmd}\""));
        if !args.is_empty() {
            let args_toml = args.iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("args = [{args_toml}]"));
        }
    }
    if let Some(env_name) = auth_env {
        lines.push(String::new());
        lines.push(format!("# Token read from env var at runtime (never stored in plain text)"));
        lines.push(format!("auth_env = \"{env_name}\""));
    }
    if let Some(ns) = namespace {
        lines.push(format!("tool_namespace = \"{ns}\""));
    }

    lines.push(format!("timeout_secs = {timeout}"));
    lines.push("enabled = true".to_string());

    lines.join("\n") + "\n"
}
```

### 2.8 `mcp_list`

```rust
async fn mcp_list() {
    let dir = crate::core::paths::get_base_dir().join("mcp-servers");
    if !dir.exists() {
        println!("No MCP servers registered (mcp-servers/ directory not found).");
        return;
    }

    let entries: Vec<_> = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().collect())
        .unwrap_or_default();

    if entries.is_empty() {
        println!("No MCP servers registered.");
        return;
    }

    println!("{:<20} {:<10} {:<8} {}", "ID", "TRANSPORT", "ENABLED", "URL / COMMAND");
    println!("{}", "-".repeat(72));

    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") { continue; }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = toml::from_str::<crate::adapters::mcp::registry::McpServerConfig>(&content) {
                let endpoint = cfg.url.as_deref()
                    .or_else(|| cfg.command.as_deref())
                    .unwrap_or("—");
                println!(
                    "{:<20} {:<10} {:<8} {}",
                    cfg.id,
                    cfg.transport,
                    if cfg.enabled { "yes" } else { "no" },
                    endpoint
                );
            }
        }
    }
}
```

### 2.9 `mcp_check`

```rust
async fn mcp_check(id: &str) {
    let path = crate::core::paths::get_base_dir()
        .join("mcp-servers")
        .join(format!("{}.toml", id));

    if !path.exists() {
        println!("Server '{}' not registered (no file: {}).", id, path.display());
        return;
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => { println!("Error reading config: {}", e); return; }
    };

    let cfg: crate::adapters::mcp::registry::McpServerConfig = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => { println!("Error parsing config: {}", e); return; }
    };

    println!("Checking '{}' ({})...", cfg.id, cfg.transport);

    match cfg.transport.as_str() {
        "http" | "sse" | "streaming" => {
            let url = cfg.url.as_deref().unwrap_or("");
            let auth_env = cfg.auth_env.as_deref();
            match check_http_mcp(url, auth_env, cfg.timeout_secs).await {
                Ok(n) => println!("✅ OK — {} tools discovered", n),
                Err(e) => println!("❌ Failed: {}", e),
            }
        }
        "stdio" => {
            let cmd = cfg.command.as_deref().unwrap_or("");
            match check_stdio_command(cmd) {
                Ok(_) => println!("✅ Command '{}' found in PATH", cmd),
                Err(e) => println!("❌ Failed: {}", e),
            }
        }
        other => println!("Unknown transport: {}", other),
    }
}
```

### 2.10 `mcp_remove`

```rust
async fn mcp_remove(id: &str) {
    let path = crate::core::paths::get_base_dir()
        .join("mcp-servers")
        .join(format!("{}.toml", id));

    if !path.exists() {
        println!("Server '{}' not registered.", id);
        return;
    }

    match std::fs::remove_file(&path) {
        Ok(_) => println!("✅ Removed: {}", path.display()),
        Err(e) => println!("Error removing config: {}", e),
    }
}
```

---

## 3. Subtarea 7.3 — Plugin contract: `docs/MCP_PLUGIN_CONTRACT.md`

Crea `docs/MCP_PLUGIN_CONTRACT.md` con el siguiente contenido:

```markdown
# Llama-R MCP Plugin Contract v1.0

> Versión: 1.0 · Fecha: [fecha actual]  
> Audiencia: desarrolladores de apps externas (Fudi, etc.) que quieren integrarse con llama-r via MCP.

## 1. Qué es este contrato

Llama-R actúa como **gateway central de agentes**. Las apps externas NO se integran directamente en el core — se integran como **MCP Servers**: procesos independientes que exponen herramientas via el protocolo MCP. Llama-r actúa como **MCP Client** y descubre + ejecuta esas herramientas.

Este documento define:
1. El protocolo que tu MCP server debe implementar.
2. Los requisitos de seguridad (autenticación, namespacing, errores).
3. Cómo registrar tu server en llama-r.
4. Cómo los agentes declaran acceso a tus herramientas.

## 2. Protocolo requerido

Tu MCP server debe implementar el **MCP JSON-RPC protocol v2024-11** sobre HTTP (recomendado) o stdio.

### 2.1 Endpoints obligatorios (HTTP transport)

```
POST <tu-url>
  Content-Type: application/json
  Body: { "jsonrpc": "2.0", "id": <int>, "method": "<método>", "params": <obj> }
```

| Método | Descripción |
|--------|-------------|
| `tools/list` | Devuelve todas las tools disponibles |
| `tools/call` | Ejecuta una tool específica |
| `initialize` | (Opcional) Handshake de negociación de capacidades |

### 2.2 Respuesta de `tools/list`

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "tools": [
      {
        "name": "list_orders",
        "description": "Lista pedidos activos del restaurante",
        "inputSchema": {
          "type": "object",
          "properties": {
            "status": { "type": "string", "enum": ["pending", "active", "completed"] }
          }
        }
      }
    ]
  }
}
```

### 2.3 Respuesta de `tools/call`

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "content": { "orders": [...] },
    "isError": false
  }
}
```

En caso de error:
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "content": { "error": "Order not found" },
    "isError": true
  }
}
```

## 3. Requisitos de seguridad

### 3.1 Autenticación

Si tu server requiere auth, acepta **Bearer token** en el header `Authorization`. El token se configura en llama-r como referencia a env var (`auth_env`), **nunca como valor literal en TOML**.

Ejemplo de config en llama-r:
```toml
auth_env = "FUDI_MCP_TOKEN"
```

Tu server lee el token del header; llama-r lo inyecta automáticamente.

### 3.2 Namespacing de tools

Usa **nombres únicos y descriptivos** para tus tools. Llama-r aplica el `tool_namespace` como prefijo (`fudi/list_orders`). Las tools deben:
- No usar nombres genéricos (`get`, `create`, `delete`) sin prefijo de dominio.
- Ser idempotentes siempre que sea posible.
- Documentar efectos secundarios en el campo `description`.

### 3.3 Datos sensibles

**Nunca retornes** en el resultado de una tool:
- Contraseñas, tokens, API keys.
- PII completa (nombres + documentos + datos bancarios combinados).
- Información de otros tenants no relacionados con el request.

### 3.4 Rate limiting propio

Tu server debe implementar su propio rate limiting. Llama-r puede tener múltiples agentes llamando a tus tools simultáneamente.

## 4. Registro en llama-r

```bash
# HTTP transport
llama-r mcp register fudi \
  --url http://localhost:4100/mcp \
  --namespace fudi \
  --auth-env FUDI_MCP_TOKEN

# stdio transport (proceso local)
llama-r mcp register filesystem \
  --command npx \
  --args -y @modelcontextprotocol/server-filesystem /data/readonly \
  --namespace fs
```

Esto crea `mcp-servers/fudi.toml` que llama-r carga automáticamente (hot reload incluido).

## 5. Declaración en agentes

```toml
# contextos/projects/mi-proyecto/agents/ops.toml
mcp_sources    = ["fudi"]
tools_override = [
  "fudi/list_orders",
  "fudi/get_order",
  "fudi/update_order_status",
]
```

Reglas de acceso:
- Un agente **solo** puede llamar tools listadas en `tools_override` de servers en `mcp_sources`.
- `tools_override = ["*"]` permite todas las tools del namespace (usar con cuidado).
- `tools_override = []` → deny-all (ninguna tool disponible, aunque `mcp_sources` no esté vacío).

## 6. Checklist de tu MCP server

- [ ] Implementa `tools/list` con schema JSON válido por tool
- [ ] Implementa `tools/call` con respuesta `{ content, isError }`
- [ ] Responde a Bearer token en `Authorization` header (si requiere auth)
- [ ] `isError: true` para errores de negocio (no excepciones no manejadas)
- [ ] Tools tienen `description` clara en español o inglés
- [ ] No retorna secretos ni PII en los resultados
- [ ] Tiene health check (cualquier endpoint que retorne 200)
- [ ] Documentado con qué env var configura el token
```

---

## 4. Subtarea 7.4 — `OpenAICompatibleProvider` adapter

### 4.1 Feature flag en `Cargo.toml`

```toml
[features]
default = ["rig-engine", "rag", "history"]
rig-engine      = ["dep:rig-core"]
rag             = []
history         = ["dep:rusqlite", "dep:uuid"]
openai-provider = []   # ← nuevo
```

> No se necesitan crates externos adicionales — usa `reqwest` (ya presente) y `serde_json`.

### 4.2 Crear `src/providers/openai_compat.rs`

```rust
//! OpenAI-compatible provider adapter.
//!
//! Talks the `/v1/chat/completions` protocol, compatible with:
//!   - OpenAI API
//!   - Anthropic (via compatibility layer)
//!   - LM Studio, vLLM, Ollama `/v1/`, Mistral, Together AI, etc.
//!
//! Feature-gated: `openai-provider`. Without this feature, this module
//! is excluded from compilation.

use crate::domain::models::{ChatMessage, ChatRequest, ChatResponse, ChatStreamEvent, ModelInfo};
use crate::providers::LLMProvider;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::pin::Pin;
use tokio_stream::Stream;

pub struct OpenAICompatibleProvider {
    client: Client,
    base_url: String,          // e.g. "https://api.openai.com" or "http://localhost:1234"
    api_key: Option<String>,   // Bearer token
    default_model: String,     // e.g. "gpt-4o-mini"
}

impl OpenAICompatibleProvider {
    /// Create from explicit values (used in tests and wiring).
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            default_model: default_model.into(),
        }
    }

    /// Create from environment variables.
    ///
    /// Reads:
    ///   LLM_BASE_URL   (default: "https://api.openai.com")
    ///   LLM_API_KEY    (or OPENAI_API_KEY as fallback)
    ///   LLM_MODEL      (default: "gpt-4o-mini")
    pub fn from_env() -> Result<Self, String> {
        let base_url = std::env::var("LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com".to_string());

        let api_key = std::env::var("LLM_API_KEY")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();

        let default_model = std::env::var("LLM_MODEL")
            .unwrap_or_else(|_| "gpt-4o-mini".to_string());

        Ok(Self::new(base_url, api_key, default_model))
    }

    fn auth_header(&self) -> Option<String> {
        self.api_key.as_ref().map(|k| format!("Bearer {k}"))
    }
}
```

### 4.3 Tipos internos OpenAI

```rust
#[derive(Serialize)]
struct OpenAIChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAIMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize)]
struct OpenAIMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OpenAIChatResponse {
    model: String,
    choices: Vec<OpenAIChoice>,
}

#[derive(Deserialize)]
struct OpenAIChoice {
    message: OpenAIChoiceMessage,
}

#[derive(Deserialize)]
struct OpenAIChoiceMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAIModelsResponse {
    data: Vec<OpenAIModel>,
}

#[derive(Deserialize)]
struct OpenAIModel {
    id: String,
}
```

### 4.4 Implementar `LLMProvider`

```rust
#[async_trait]
impl LLMProvider for OpenAICompatibleProvider {
    fn get_base_url(&self) -> String {
        self.base_url.clone()
    }

    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let url = format!("{}/v1/models", self.base_url);
        let mut req = self.client.get(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.timeout(std::time::Duration::from_secs(5)).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("Provider health check failed: HTTP {}", resp.status()).into())
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        let url = format!("{}/v1/models", self.base_url);
        let mut req = self.client.get(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        let resp = req.timeout(std::time::Duration::from_secs(10)).send().await?;
        let data: OpenAIModelsResponse = resp.json().await?;
        Ok(data.data.into_iter().map(|m| ModelInfo {
            name: m.id,
            modified_at: String::new(),
            size: 0,
        }).collect())
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let model = if request.model.is_empty() { &self.default_model } else { &request.model };

        let messages: Vec<OpenAIMessage> = request.messages.iter().map(|m| OpenAIMessage {
            role: &m.role,
            content: &m.content,
        }).collect();

        let body = OpenAIChatRequest { model, messages, stream: false, temperature: None };

        let mut req = self.client.post(&url)
            .timeout(std::time::Duration::from_secs(120))
            .json(&body);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }

        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("OpenAI API error {status}: {text}").into());
        }

        let oai_resp: OpenAIChatResponse = resp.json().await?;
        let choice = oai_resp.choices.into_iter().next()
            .ok_or("OpenAI returned no choices")?;

        Ok(ChatResponse {
            model: oai_resp.model,
            created_at: chrono::Utc::now().to_rfc3339(),
            message: ChatMessage {
                role: choice.message.role,
                content: choice.message.content,
            },
            done: true,
        })
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatStreamEvent, Box<dyn Error + Send + Sync>>> + Send>>, Box<dyn Error + Send + Sync>> {
        // SSE streaming from OpenAI /v1/chat/completions with stream=true
        // Parse `data: {"choices":[{"delta":{"content":"..."}}]}` lines
        // Emit ChatStreamEvent per chunk; emit done=true on `data: [DONE]`
        //
        // Implementation uses reqwest streaming + tokio_stream::wrappers::LinesStream
        // Full implementation: see §4.5
        todo!("implement streaming")
    }
}
```

### 4.5 Streaming SSE

El streaming de OpenAI envía líneas `data: <json>` separadas por `\n\n`. La implementación:

```rust
async fn chat_stream(&self, request: ChatRequest) -> Result<...> {
    use tokio_stream::StreamExt;
    use futures::TryStreamExt;

    let url = format!("{}/v1/chat/completions", self.base_url);
    let model = if request.model.is_empty() { &self.default_model } else { &request.model };

    let messages: Vec<OpenAIMessage> = request.messages.iter()
        .map(|m| OpenAIMessage { role: &m.role, content: &m.content })
        .collect();

    let body = OpenAIChatRequest { model, messages, stream: true, temperature: None };

    let mut req = self.client.post(&url).json(&body);
    if let Some(auth) = self.auth_header() {
        req = req.header("Authorization", auth);
    }

    let resp = req.send().await.map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
    let model_name = model.to_string();

    // Convert the byte stream to line-by-line SSE parsing
    let stream = resp.bytes_stream()
        .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>);

    // Collect bytes, split on newlines, parse SSE events
    // Each `data:` line is a JSON delta; `data: [DONE]` signals end.
    let event_stream = async_stream::try_stream! {
        let mut buffer = String::new();
        let mut byte_stream = Box::pin(stream);

        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline_pos) = buffer.find('\n') {
                let line = buffer[..newline_pos].trim().to_string();
                buffer = buffer[newline_pos + 1..].to_string();

                if line.starts_with("data: ") {
                    let data = &line["data: ".len()..];
                    if data == "[DONE]" {
                        yield ChatStreamEvent {
                            model: model_name.clone(),
                            created_at: chrono::Utc::now().to_rfc3339(),
                            message: ChatMessage { role: "assistant".to_string(), content: String::new() },
                            done: true,
                        };
                        return;
                    }
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(content) = json["choices"][0]["delta"]["content"].as_str() {
                            yield ChatStreamEvent {
                                model: model_name.clone(),
                                created_at: chrono::Utc::now().to_rfc3339(),
                                message: ChatMessage { role: "assistant".to_string(), content: content.to_string() },
                                done: false,
                            };
                        }
                    }
                }
            }
        }
    };

    Ok(Box::pin(event_stream))
}
```

### 4.6 Wiring en `runtime.rs`

Agrega la lógica de selección de provider en `build_runtime`, antes de construir `provider_impl`:

```rust
// In build_runtime, after config load:

let provider_impl: Arc<dyn LLMProvider + Send + Sync> = select_provider(&config);

fn select_provider(config: &Config) -> Arc<dyn LLMProvider + Send + Sync> {
    let explicit = std::env::var("LLM_PROVIDER")
        .unwrap_or_default()
        .to_ascii_lowercase();

    #[cfg(feature = "openai-provider")]
    if explicit == "openai" || explicit == "openai-compat" {
        match crate::providers::openai_compat::OpenAICompatibleProvider::from_env() {
            Ok(p) => {
                tracing::info!(base_url = %p.get_base_url(), "Using OpenAI-compatible provider");
                return Arc::new(p);
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to init OpenAI provider; falling back to Ollama");
            }
        }
    }

    // Default: Ollama
    Arc::new(crate::providers::ollama::OllamaProvider::new(config.ollama_url.clone()))
}
```

### 4.7 Registrar módulo en `src/providers/mod.rs`

```rust
pub mod ollama;

#[cfg(feature = "openai-provider")]
pub mod openai_compat;
```

### 4.8 Variables de entorno nuevas

Agrega a `src/config.rs`:

```rust
// No se necesita agregar a Config struct — se lee directamente en select_provider
// Documentar en .env.example:
```

Agrega a `.env.example`:

```dotenv
# Multi-provider support (requires feature openai-provider at compile time)
# LLM_PROVIDER=openai   # "openai" or "ollama" (default: "ollama")
# LLM_BASE_URL=https://api.openai.com  # or http://localhost:1234 for LM Studio
# LLM_API_KEY=sk-...    # API key (or OPENAI_API_KEY)
# LLM_MODEL=gpt-4o-mini # Default model for the OpenAI-compatible provider
```

---

## 5. Subtarea 7.5 — Dockerfile + docker-compose.yml

### 5.1 `Dockerfile` (multi-stage)

Crea `Dockerfile` en la raíz del repo:

```dockerfile
# ── Stage 1: Build ────────────────────────────────────────────────────────────
FROM rust:1.82-slim AS builder

# Install protoc (needed by build.rs for tonic/prost)
RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependencies separately (layer caching optimization)
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY proto/ ./proto/
# Create dummy src to build deps
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --features "rig-engine,rag,history" 2>/dev/null || true
RUN rm -rf src

# Copy real source and build
COPY src/ ./src/
RUN touch src/main.rs  # force rebuild
RUN cargo build --release --features "rig-engine,rag,history"

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# Runtime deps: ca-certs for HTTPS to Ollama/external APIs
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root user
RUN useradd -r -s /bin/false -u 1001 llamar
USER llamar

WORKDIR /app

# Copy binary
COPY --from=builder /build/target/release/llama-r /app/llama-r

# Persistent data directories (override with LLAMA_R_DIR or volume mounts)
RUN mkdir -p /app/data/lancedb /app/agents /app/contextos /app/mcp-servers /app/logs

# Default environment
ENV PORT=3000
ENV OLLAMA_URL=http://ollama:11434
ENV LLAMA_R_DIR=/app

EXPOSE 3000
EXPOSE 50051

ENTRYPOINT ["/app/llama-r"]
CMD ["run"]
```

### 5.2 `docker-compose.yml`

Crea `docker-compose.yml` en la raíz del repo:

```yaml
version: "3.9"

services:
  llama-r:
    build:
      context: .
      dockerfile: Dockerfile
    image: llama-r:latest
    container_name: llama-r
    restart: unless-stopped
    ports:
      - "3000:3000"    # HTTP API
      - "50051:50051"  # gRPC
    environment:
      - PORT=3000
      - OLLAMA_URL=http://ollama:11434
      # Set DEFAULT_MODEL or let interactive setup run on first start
      # - DEFAULT_MODEL=llama3.2
      - LLAMA_R_DIR=/app
      - RAG_ENABLED=true
      - EMBEDDING_MODEL=nomic-embed-text
      - EMBEDDING_DIMENSIONS=768
      # Rate limiting (optional)
      # - RATE_LIMIT_RPM=120
      # History retention
      # - HISTORY_RETENTION_DAYS=90
    volumes:
      # Persistent data (SQLite history, RAG vectors)
      - llama-r-data:/app/data
      # Agent configs (editable without rebuild)
      - ./agents:/app/agents
      # Project contexts
      - ./contextos:/app/contextos
      # MCP server configs
      - ./mcp-servers:/app/mcp-servers
      # Logs
      - llama-r-logs:/app/logs
    depends_on:
      ollama:
        condition: service_healthy
    healthcheck:
      test: ["CMD", "wget", "-qO-", "http://localhost:3000/health"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 10s

  ollama:
    image: ollama/ollama:latest
    container_name: ollama
    restart: unless-stopped
    ports:
      - "11434:11434"
    volumes:
      - ollama-models:/root/.ollama
    healthcheck:
      test: ["CMD", "wget", "-qO-", "http://localhost:11434/api/tags"]
      interval: 10s
      timeout: 5s
      retries: 5
      start_period: 20s

volumes:
  llama-r-data:
    driver: local
  llama-r-logs:
    driver: local
  ollama-models:
    driver: local
```

### 5.3 `.dockerignore`

Crea `.dockerignore` en la raíz del repo:

```
target/
target-tests/
.git/
.env
data/
logs/
*.log
```

### 5.4 Notas de uso en README.md (sección Docker)

La sección Docker del README (ver §6) debe incluir:

```markdown
## Docker

### Inicio rápido

```bash
# 1. Copiar configuración
cp .env.example .env
# Editar .env con DEFAULT_MODEL (o dejarlo vacío para setup interactivo)

# 2. Levantar servicios (llama-r + Ollama)
docker compose up -d

# 3. Verificar salud
curl http://localhost:3000/health

# 4. Descargar un modelo en Ollama (primera vez)
docker exec ollama ollama pull llama3.2
```

### Volúmenes persistentes

| Volumen | Contenido |
|---------|-----------|
| `./agents` | TOMLs de agentes globales (bind mount, editable en caliente) |
| `./contextos` | Contextos de proyectos y agentes de proyecto |
| `./mcp-servers` | TOMLs de servidores MCP |
| `llama-r-data` | SQLite history + vectores RAG (volumen Docker) |
| `llama-r-logs` | Logs rotativos (volumen Docker) |

### Build personalizado

```bash
# Compilar con features específicas
docker build --build-arg FEATURES="rig-engine,rag,history,openai-provider" -t llama-r:custom .
```
```

---

## 6. Subtarea 7.6 — Documentación final

### 6.1 Ampliar `export-rules` con sección manifest

En `src/cli/commands.rs`, función `run_export_rules`, después de generar el contenido base:

```rust
// Append agent manifest section for IDE AI context
let agent_manifest_section = build_agent_manifest_section(project_id, &ctx);

let final_content = format!(
    "# {}\n# Generado por Llama-R para el proyecto: {}\n\n{}\n\n{}",
    header, ctx.project_id, ctx.context_md, agent_manifest_section
);
```

```rust
fn build_agent_manifest_section(project_id: &str, _ctx: &crate::context::store::ProjectContext) -> String {
    // Try to read the project default agent TOML
    let agent_path = crate::core::paths::get_project_agents_dir(project_id)
        .join(format!("{}.toml", project_id));

    let agent_info = if agent_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&agent_path) {
            if let Ok(cfg) = toml::from_str::<crate::domain::agent::AgentConfig>(&content) {
                let mcp_sources = if cfg.mcp_sources.is_empty() {
                    "none".to_string()
                } else {
                    cfg.mcp_sources.join(", ")
                };
                let tools = if cfg.tools_override.is_empty() {
                    "deny-all".to_string()
                } else {
                    cfg.tools_override.join(", ")
                };
                format!(
                    "- **Model**: {}\n- **MCP Sources**: {}\n- **Allowed Tools**: {}\n- **RAG Sources**: {}",
                    if cfg.model.is_empty() { "DEFAULT_MODEL".to_string() } else { cfg.model },
                    mcp_sources,
                    tools,
                    if cfg.rag_sources.is_empty() { "none".to_string() } else { cfg.rag_sources.join(", ") }
                )
            } else { "Agent config parse error".to_string() }
        } else { "Agent config not readable".to_string() }
    } else {
        "No default agent configured".to_string()
    };

    format!(
        "## Llama-R Agent Manifest\n\nProject: `{}`\n\n{}\n\nChat endpoint: `POST http://localhost:3000/api/chat`\nHeaders: `X-Project: {}` · `X-Agent: <agent_id>`",
        project_id, agent_info, project_id
    )
}
```

### 6.2 Actualizar `README.md`

El `README.md` debe reflejar el estado final de la plataforma tras todas las fases. Reescribe las secciones:

**Variables de entorno** — agregar todas las nuevas:

| Variable | Default | Descripción |
|---|---|---|
| `PORT` | `3000` | Puerto HTTP |
| `OLLAMA_URL` | `http://localhost:11434` | URL del provider Ollama |
| `DEFAULT_MODEL` | — | Modelo por defecto (obligatorio) |
| `LLAMA_R_DIR` | — | Directorio base override para datos |
| `EMBEDDING_MODEL` | `nomic-embed-text` | Modelo de embeddings para RAG |
| `EMBEDDING_DIMENSIONS` | `768` | Dimensiones del vector de embedding |
| `RAG_ENABLED` | `true` | Habilita el store RAG |
| `HISTORY_RETENTION_DAYS` | `90` | Días de retención del historial |
| `RATE_LIMIT_RPM` | `0` | Rate limit (0 = deshabilitado) |
| `RATE_LIMIT_BURST` | `10` | Burst del rate limiter |
| `LLM_PROVIDER` | `ollama` | Provider LLM (`ollama` o `openai`) |
| `LLM_BASE_URL` | `https://api.openai.com` | URL base para OpenAI-compatible |
| `LLM_API_KEY` | — | API key del provider OpenAI-compatible |
| `LLM_MODEL` | `gpt-4o-mini` | Modelo por defecto del provider OpenAI |

**CLI** — agregar nuevos comandos:

```bash
# Nuevo: agente con scope v2 completo
llama-r init-agent mi-agente --full

# Nuevo: gestión de MCP servers
llama-r mcp register fudi --url http://localhost:4100/mcp --namespace fudi --auth-env FUDI_MCP_TOKEN
llama-r mcp list
llama-r mcp check fudi
llama-r mcp remove fudi
```

**HTTP API** — agregar endpoints nuevos de Fases 5 y 6:

```
GET    /api/conversations
GET    /api/conversations/:id
GET    /api/conversations/:id/messages
DELETE /api/conversations/:id
POST   /api/conversations/:id/export
GET    /api/metrics   (X-Debug: true)
POST   /api/rag/ingest  (X-Debug: true)
POST   /api/rag/query   (X-Debug: true)
```

**Storage layout** — actualizar para incluir `data/`:

```
<base_dir>/
├── agents/                     # Agentes globales
│   └── *.toml
├── contextos/
│   └── projects/
│       └── <project_id>/
│           ├── agents/         # Agentes del proyecto
│           ├── context/        # Contexto generado
│           └── rag/            # (metadata RAG local, futuro)
├── mcp-servers/                # Config MCP servers
│   └── *.toml
├── data/                       # Runtime data (gitignored)
│   ├── history.db              # SQLite: conversaciones y mensajes
│   └── lancedb/                # Vectores RAG por colección
│       └── <encoded_source_id>/
│           └── docs.jsonl
└── logs/
    └── llama-r.log.YYYY-MM-DD
```

### 6.3 Actualizar `AGENTS.md`

Agrega en **Core Commands**:

```powershell
# Crear agente con scope completo v2
cargo run -- init-agent mi-agente --full

# Registrar un MCP server HTTP
cargo run -- mcp register fudi --url http://localhost:4100/mcp --namespace fudi

# Listar servidores MCP registrados
cargo run -- mcp list

# Verificar conectividad de un servidor
cargo run -- mcp check fudi

# Eliminar un servidor
cargo run -- mcp remove fudi
```

Agrega en **HTTP API Quick Reference** todas las rutas nuevas de Fases 5–7.

Actualiza la tabla de **Plataforma extensible** en `ROADMAP.md`:

```markdown
| 7 | Ecosistema (CLI, Docker, docs) | P2 | ✅ completado |
```

---

## 7. Estructura de módulos final

Después de Fase 7, el repo queda así:

```text
raíz/
├── Dockerfile              ← multi-stage build (NUEVO)
├── docker-compose.yml      ← Ollama + llama-r (NUEVO)
├── .dockerignore           ← excluye target/, data/, .env (NUEVO)
│
src/
├── cli/
│   └── commands.rs         ← init-agent --full + Commands::Mcp (modificar)
│
├── providers/
│   ├── mod.rs              ← registrar openai_compat (modificar)
│   ├── ollama.rs           ← sin cambios
│   └── openai_compat.rs    ← OpenAICompatibleProvider (NUEVO)
│
└── runtime.rs              ← select_provider() (modificar)

docs/
├── adrs/                   ← sin cambios
├── THREAT_MODEL.md         ← Fase 6
└── MCP_PLUGIN_CONTRACT.md  ← (NUEVO)

README.md                   ← actualización completa (modificar)
AGENTS.md                   ← actualización completa (modificar)
ROADMAP.md                  ← Fase 7 completado (modificar)
.env.example                ← variables multi-provider (modificar)
```

---

## 8. Tests de Fase 7

### 8.1 `init-agent --full` produce TOML válido

```rust
#[test]
fn init_agent_full_toml_parses_correctly() {
    // Invoke init_agent_file_v2 to a tempdir and parse the result
    // Verify all v2 scope fields are present with safe defaults
}
```

### 8.2 `mcp register` genera TOML correcto

```rust
#[test]
fn build_mcp_server_toml_http() {
    let toml = build_mcp_server_toml(
        "fudi", "http", Some("http://localhost:4100/mcp"),
        None, &[], Some("fudi"), Some("FUDI_TOKEN"), 30
    );
    let cfg: crate::adapters::mcp::registry::McpServerConfig = toml::from_str(&toml).unwrap();
    assert_eq!(cfg.id, "fudi");
    assert_eq!(cfg.transport, "http");
    assert_eq!(cfg.auth_env.as_deref(), Some("FUDI_TOKEN"));
    assert_eq!(cfg.tool_namespace.as_deref(), Some("fudi"));
    assert!(cfg.enabled);
}

#[test]
fn build_mcp_server_toml_stdio() {
    let args = vec!["--arg1".to_string(), "--arg2".to_string()];
    let toml = build_mcp_server_toml(
        "fs", "stdio", None, Some("npx"),
        &args, Some("fs"), None, 30
    );
    let cfg: crate::adapters::mcp::registry::McpServerConfig = toml::from_str(&toml).unwrap();
    assert_eq!(cfg.command.as_deref(), Some("npx"));
    assert_eq!(cfg.args.len(), 2);
}
```

### 8.3 `OpenAICompatibleProvider` (sin red)

```rust
#[test]
fn openai_provider_from_env_uses_defaults() {
    // Unset env vars and verify defaults are applied
    std::env::remove_var("LLM_BASE_URL");
    std::env::remove_var("LLM_API_KEY");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("LLM_MODEL");

    #[cfg(feature = "openai-provider")]
    {
        let p = crate::providers::openai_compat::OpenAICompatibleProvider::from_env().unwrap();
        assert_eq!(p.get_base_url(), "https://api.openai.com");
    }
}

#[tokio::test]
#[ignore = "requires live OpenAI-compatible endpoint at LLM_BASE_URL"]
async fn openai_provider_chat_integration() {
    #[cfg(feature = "openai-provider")]
    {
        let p = crate::providers::openai_compat::OpenAICompatibleProvider::from_env().unwrap();
        let req = crate::domain::models::ChatRequest {
            model: "gpt-4o-mini".to_string(),
            messages: vec![crate::domain::models::ChatMessage {
                role: "user".to_string(),
                content: "Say exactly: hello".to_string(),
            }],
            stream: false,
        };
        let resp = p.chat(req).await.unwrap();
        assert!(!resp.message.content.is_empty());
    }
}
```

---

## 9. Secuencia de validación paso a paso

```bash
# Después de 7.1 (init-agent --full):
cargo check
cargo run -- init-agent test-full --full
# Verifica que el archivo creado es TOML válido

# Después de 7.2 (mcp commands):
cargo check
cargo run -- mcp list
cargo run -- mcp register test-server --url http://localhost:9999/mcp --force
cargo run -- mcp list
cargo run -- mcp remove test-server

# Después de 7.3 (contract doc):
ls docs/MCP_PLUGIN_CONTRACT.md

# Después de 7.4 (OpenAI provider):
cargo check
cargo check --features openai-provider

# Después de 7.5 (Docker):
docker build -t llama-r:test . --no-cache
# O si no hay Docker local: solo verificar que el Dockerfile tiene sintaxis válida

# Después de 7.6 (docs):
cargo check
cargo test --target-dir target-tests

# Final:
cargo fmt
cargo check
cargo check --features "rig-engine,rag,history,openai-provider"
cargo test --target-dir target-tests
```

---

## 10. Notas de diseño importantes

### 10.1 Por qué el adapter OpenAI no es el default

Ollama sigue siendo el provider default porque:
1. Es el provider documentado y testeado para el uso personal local.
2. No requiere API key ni egress de red.
3. El flujo interactivo de setup ya lo maneja.

El adapter OpenAI-compatible es **opt-in** via `LLM_PROVIDER=openai` + `LLM_API_KEY`.

### 10.2 Por qué `mcp register` valida antes de guardar

El error más frecuente al integrar un MCP server es una URL incorrecta o el server no corriendo. Validar en el momento del registro da feedback inmediato. El flag `--force` existe para casos donde el server no está corriendo pero el operador quiere pre-configurarlo.

### 10.3 `init-agent --full` vs default

El template simple (sin `--full`) sigue siendo el default para no abrumar a nuevos usuarios con campos desconocidos. El `--full` es para operadores que ya entienden el modelo de scope y quieren el scaffold completo desde el inicio.

### 10.4 Docker y el setup interactivo

Si `DEFAULT_MODEL` no está configurado en el entorno, llama-r intenta el setup interactivo al arrancar. En Docker esto falla porque no hay TTY. El operador debe:
1. Configurar `DEFAULT_MODEL` en el `docker-compose.yml` → `environment`.
2. O montar un `.env` configurado como volumen.

Documenta esto claramente en el README.

### 10.5 `export-rules` y la sección manifest

La sección `## Llama-R Agent Manifest` al final de cada formato exportado ayuda a los IDEs AI (Cursor, Gemini, Claude) a entender el agente que está activo en el proyecto, sin que el desarrollador tenga que copiar esa info manualmente. Es puro valor documentacional — no afecta el comportamiento del runtime.
