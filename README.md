# Llama-R: Personal AI Gateway

[![CI](https://github.com/LEMNXCIX/llama-r/actions/workflows/ci.yml/badge.svg)](https://github.com/LEMNXCIX/llama-r/actions/workflows/ci.yml)

Llama-R es un gateway AI personal escrito en Rust que se sitúa frente a un provider de LLM (Ollama por defecto, o `openrouter`, `opencode`, `openai-compatible`, `anthropic` y `gemini`) y expone APIs unificadas para chatear con agentes especializados. Cada agente puede tener su propio provider y modelo, prompt de sistema, contexto de proyecto, herramientas MCP, reglas de optimización y política de acceso RAG.

## Arquitectura

```
Cliente HTTP/gRPC/MCP → Llama-R → Provider (Ollama por defecto, o uno de los cinco remotos)
                            ↓
                    AgentRegistry
                    ScopeBuilder
                    ContextEnricher
                    TokenOptimizer
```

El runtime levanta tres servidores simultáneamente:
- **HTTP API** (puerto `3000` por defecto)
- **gRPC** (puerto `50051`)
- **TUI** (terminal)

## Requisitos

- Rust y Cargo
- Un provider accesible: [Ollama](https://ollama.ai) local para el valor por defecto (`http://127.0.0.1:11434`), o credenciales para OpenRouter/OpenCode/OpenAI-compatible/Anthropic/Gemini. El provider caído no impide el arranque: `/api/health` reporta `degraded` y se recupera solo.

## Configuración rápida

```bash
cp .env.example .env
# Editar .env con OLLAMA_URL y DEFAULT_MODEL, o dejar que el setup interactivo lo haga
cargo run
```

En el primer arranque, si falta `DEFAULT_MODEL`, Llama-R abre un setup interactivo y persiste la configuración en `.env`.

### Variables de entorno

| Variable | Default | Descripción |
|---|---|---|
| `PORT` | `3000` | Puerto HTTP |
| `PROVIDER` | `ollama` | Provider de arranque para chat, enrutado y embeddings: `ollama` \| `openrouter` \| `opencode` \| `openai-compatible` \| `anthropic` \| `gemini`. Un agente puede sobreescribirlo con su campo `provider` |
| `OLLAMA_URL` | `http://localhost:11434` | URL de Ollama (también fallback de embeddings preferido para providers sin endpoint propio: `opencode`, `anthropic`; si está vacía se usa `OPENAI_BASE_URL` + `OPENROUTER_API_KEY`) |
| `OPENROUTER_API_KEY` | — | Clave Bearer de OpenRouter; se guarda en `.env` (modo `0600`), nunca en URLs/logs/health/pantalla |
| `OPENAI_API_KEY` | — | Clave Bearer para `openai-compatible` (Groq/Mistral/DeepSeek/xAI/gateways propios) |
| `ANTHROPIC_API_KEY` | — | Clave de Anthropic; viaja en la cabecera `x-api-key`, nunca en la URL |
| `GEMINI_API_KEY` | — | Clave de Gemini; viaja en la cabecera `x-goog-api-key` (nunca `?key=` en la URL) |
| `OPENAI_BASE_URL` | `https://openrouter.ai/api/v1` | Base OpenAI-compatible para OpenRouter y `openai-compatible` (este último no tiene default fijo); con `OPENROUTER_API_KEY` también sirve de fallback de embeddings para `opencode`/`anthropic` |
| `OPENCODE_API_KEY` | — | Contraseña del servidor OpenCode (HTTP basic auth); misma regla de secreto que las demás claves |
| `OPENCODE_URL` | `http://127.0.0.1:4096` | Origen de los endpoints generate/decisión de OpenCode |
| `DEFAULT_MODEL` | — | Modelo por defecto (obligatorio) |
| `LLAMA_R_DIR` | — | Directorio base override para datos |
| `ADMIN_TOKEN` | — (fail-closed) | Secreto compartido para `X-Admin-Token` en las rutas admin; vacío/ausente = 401 en todas. En `.env` (modo `0600`), nunca en logs ni respuestas |
| `APPROVAL_PARK` | `true` | `1`/`true`/`yes`/`on` = aprobación sin deadline; `false` restaura el timeout acotado |
| `APPROVAL_TIMEOUT_SECS` | `120` | Deadline del gate cuando no se aparca; `0` = sin deadline. Malformado = error de config |
| `PARK_GRACE_MS` | `1000` | Ms que espera el handler antes de responder `202` a un turno aparcado. Malformado = error de config |
| `REAPER_INTERVAL_SECS` | `5` | Periodo del reaper de huérfanos; numérico no positivo cae a `5`, malformado = error de config |
| `OTEL_ENABLED` | `false` | Export OTLP/HTTP opt-in; con `false` no se envía nada |
| `OTEL_ENDPOINT` | `http://127.0.0.1:4318/v1/traces` | URL de trazas OTLP/HTTP, solo se lee si está activado |
| `OTEL_SERVICE_NAME` | `llama-r` | Atributo `service.name` de cada span |
| `HTTP_TIMEOUT_MS` | `30000` | Timeout de las peticiones HTTP salientes (provider, health). `0` se rechaza |
| `CORS_ORIGINS` | `http://localhost,http://127.0.0.1` | Lista separada por comas de orígenes web permitidos por CORS. `*` se rechaza (usa un origen explícito); solo los listados reciben `Access-Control-Allow-Origin`. Revisar en cualquier despliegue LAN |
| `EMBEDDING_MODEL` | `nomic-embed-text` | Modelo de embeddings de Ollama para RAG |
| `EMBEDDING_DIMENSIONS` | `768` | Tamaño esperado del vector de embeddings |
| `RAG_ENABLED` | `true` | Construye/usa el almacén RAG en tiempo de ejecución |
| `HISTORY_RETENTION_DAYS` | `90` | Purga conversaciones anteriores a N días; `0` desactiva |

Las claves `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` y `GEMINI_API_KEY` son **nombres** de variables referenciados por los perfiles integrados de `providers.toml` (`api_key_env`); `Config::from_env` lee directamente `OPENROUTER_API_KEY`, `OPENCODE_API_KEY` y `OPENAI_BASE_URL`. Ninguna clave aparece en health, logs, URLs ni la TUI (los tests lo comprueban).

### Despliegue en LAN / hardening operativo

- **CORS:** para permitir un origen web añádelo a `CORS_ORIGINS` (separado por comas), p. ej. `CORS_ORIGINS=http://localhost:3000,http://192.168.1.10:3000`. El valor `*` se rechaza; solo los orígenes listados reciben `Access-Control-Allow-Origin`.
- **Historial de conversaciones:** los endpoints `/api/conversations*` ahora requieren `X-Admin-Token` (cambio incompatible). Ejemplo: `curl -H 'X-Admin-Token: <token>' http://host:3000/api/conversations?project_id=p&agent_id=a`. Un `GET /api/conversations` sin `project_id`/`agent_id` devuelve una página vacía (`[]`).
- **Tamaño de cuerpo:** las peticiones HTTP tienen un límite de 2 MiB; cuerpos mayores responden `413`.
- **Apagado:** con `--server-only`, `Ctrl-C` o `SIGTERM` drenan las peticiones en curso y salen con código 0.
- **`.env`:** se crea con modo `0600` en arranque; las claves nunca aparecen en logs ni en `/api/health`.

## Providers

`PROVIDER` elige el upstream de **arranque** para chat, router de decisión y (donde existe) embeddings; no se mezcla por servicio. Valores: `ollama` (por defecto), `openrouter`, `opencode`, `openai-compatible`, `anthropic` y `gemini` (un valor desconocido es un error de configuración que nombra los seis). **Cada agente puede sobreescribirlo**: la precedencia es `agent.provider` > `PROVIDER` global > `ollama` (y `agent.model` > `DEFAULT_MODEL`); `agent.provider` acepta un nombre canónico o un perfil de `providers.toml`. Las credenciales viven en `.env` (modo `0600` en Unix) y nunca aparecen en URLs, logs, `/api/health` ni en la TUI. El provider se elige en el setup interactivo del primer arranque y se puede cambiar en caliente desde el modal Settings de la TUI (sobre el Dashboard): Apply persiste en `.env` y reconstruye el stack, sus companion services (motor, RAG) **y el embedder de ranking de skills del `context_enricher`** sin reiniciar (la salvedad anterior ya no aplica). El modal Settings cicla los tres providers con motor en el arranque — Ollama / OpenRouter / OpenCode; `openai-compatible`, `anthropic` y `gemini` se seleccionan por `PROVIDER`/`providers.toml`.

#### Perfiles `providers.toml`

Opcional, en el directorio base. Seis perfiles integrados (`ollama`, `openrouter`, `opencode`, `openai-compatible`, `anthropic`, `gemini`) existen siempre; el archivo solo sobrescribe los campos que especifica. Cada perfil acepta `type` (uno de los seis valores de `PROVIDER`), `base_url`, `api_key_env` (**nombre** de la variable de entorno con la clave, nunca la clave) y `timeout_ms` (default `30000`; `0` se rechaza). Un archivo ausente es correcto; uno malformado se registra como aviso y se usan los perfiles integrados. Ejemplo:

```toml
[providers.my-openai]
type = "openai-compatible"
base_url = "https://api.example.com/v1"
api_key_env = "MY_OPENAI_KEY"   # nombre de la variable, no la clave
timeout_ms = 30000
```

#### Matriz de capacidades

El streaming y el uso de herramientas se deciden por la **capacidad del modelo resuelto** (`ModelCapabilities`, catálogo estático y conservador en `src/providers/capabilities.rs`), no por el nombre del provider. Un modelo desconocido o renombrado recibe todas las capacidades en `false`: nunca se asume nada. El catálogo actual marca los prefijos de Ollama con streaming + tools + embeddings; los de Anthropic, Gemini y OpenAI-compatible con streaming + tools + vision; OpenRouter con tools + embeddings pero sin streaming (501); y OpenCode sin ninguna.

| Capacidad | Ollama | OpenRouter | OpenCode | OpenAI-compatible | Anthropic | Gemini |
|---|---|---|---|---|---|---|
| Chat no-streaming | sí | sí | sí | sí | sí | sí |
| `stream: true` | sí | `501` | `501` | `501` (sin motor; ver nota\*) | `501` (sin motor; ver nota\*) | `501` (sin motor; ver nota\*) |
| Embeddings / RAG | sí | sí | no (salvo fallback Ollama u OpenAI-compatible) | sí | no (salvo fallback Ollama u OpenAI-compatible) | sí |
| Router de decisión d1 | sí (llama.cpp) | sí (nativo) | sí (nativo) | sí (llama.cpp) | sí (llama.cpp) | sí (llama.cpp) |
| Motor de agentes (Rig) | `rig-core` ollama | `rig-core` openrouter (tools/RAG/MCP) | no | cliente nativo\* | cliente nativo\* | cliente nativo\* |

\* Existen clientes Rig nativos para OpenAI-compatible, Anthropic y Gemini, pero el selector de arranque (`engine_target_for`) solo acopla motor a Ollama y OpenRouter. En los otros tres (y en OpenCode) las peticiones con agente degradan a un chat directo del provider con un aviso explícito de que tools/RAG/MCP no están disponibles; `/api/health` lo reporta como `provider.agent_engine: false` junto a `provider.agent_engine_reason`.

- **Streaming:** los modelos marcados como `streaming: true` con motor acoplado emiten tokens finales progresivamente (Nivel-A: token final + completado) **y** un evento `tool` aditivo por cada llamada y resultado de herramienta (`kind` = `call`/`result`, `tool_name` cualificado, `args_hash` en la llamada, `is_error` en el resultado); los argumentos en crudo nunca se incluyen en el stream. El nombre cualificado es `servicio__herramienta` para una herramienta MCP o el id simple para una local. Los eventos de token normales omiten las claves `tool`/`approval`, así que los clientes existentes ven la misma carga útil. Los adaptadores directos distintos de Ollama rechazan `chat_stream` con `501 Unavailable` antes de abrir conexión, así que un stream sin agente (o sin motor) sigue devolviendo 501 aunque el catálogo diga que el modelo puede hacer streaming. El descubrimiento de herramientas MCP se omite para modelos sin `tool_calling`.
- **Embeddings/RAG:** Ollama, OpenRouter, OpenAI-compatible y Gemini tienen adaptador de embeddings; Anthropic y OpenCode no. Sin ningún fallback (`OLLAMA_URL`, o `OPENAI_BASE_URL` junto con `OPENROUTER_API_KEY`), RAG queda desactivado y `/api/rag/*` responde un `501` explícito que nombra los embeddings (no un `500`). Con `OLLAMA_URL`, los embeddings de Ollama pueden servir de fallback (se prueba primero); si la URL está vacía, un endpoint OpenAI-compatible apuntado por `OPENAI_BASE_URL` y autenticado con `OPENROUTER_API_KEY` sirve los embeddings (`POST /v1/embeddings`; los orígenes de OpenRouter o con `/api/v1` usan la ruta de ese adaptador). La clave viaja en una cabecera, nunca en URLs, logs ni health.
- **Fallback acotado:** si fallan el provider principal y su reintento con `DEFAULT_MODEL`, se prueba **un único** perfil siguiente usable (en orden determinista por nombre; se omiten perfiles sin credencial en su `api_key_env`). Nunca se recorre una cadena. `ChatResponse.served_by` nombra el perfil que respondió y `ChatResponse.usage` trae los tokens del upstream (`cost_usd` solo si hay precios en el catálogo, hoy ninguno). En streaming el failover solo ocurre al abrir el stream y un 501 deliberado no se reintenta.
- **Router:** los seis providers pueden servir la decisión d1. Ollama, OpenAI-compatible, Anthropic y Gemini usan el `llama-server` aparte; OpenRouter y OpenCode tienen adaptadores nativos. El enrutado sigue **desactivado por defecto** (`ROUTER_ENABLED=false`) y no cambia cuando está off.

`/api/health` expone el objeto `provider`: `{ "name", "embeddings": bool, "reason": null|string, "agent_engine": bool, "agent_engine_reason": null|string }` — sin claves. `reason` nombra la carencia de embeddings y `agent_engine_reason` usa el mismo texto que el aviso de degradación del chat. (`usage` y `served_by` pertenecen a la respuesta de chat, no a health.)

El smoke multi-provider se ejecuta con `cargo test --test provider_eval_smoke` (modo fixture por defecto, sin clave ni red real).

## CLI

### Iniciar todo (HTTP + gRPC + TUI)
```bash
cargo run
# o explícitamente:
cargo run -- run
```

### Crear agente base del proyecto
```bash
cargo run -- init
```
Crea `contextos/projects/<directorio-actual>/agents/<directorio-actual>.toml`.

### Crear agente especializado
```bash
cargo run -- init-agent nutricion
```
Crea un agente con nombre específico para el proyecto actual.

### Analizar proyecto y generar contexto
Requiere el servidor corriendo. Usa `--server` si el puerto no es el `3000` por defecto (respeta la variable `PORT` del entorno):

```bash
cargo run -- analyze /ruta/al/proyecto --id mi-proyecto --agent nutricion
cargo run -- analyze /ruta --id mi-proyecto --server http://localhost:8082
```

### Reanalizar contexto existente
```bash
cargo run -- reanalyze mi-proyecto
cargo run -- reanalyze mi-proyecto --server http://localhost:8082
```

### Exportar reglas
```bash
cargo run -- export-rules mi-proyecto ./destino --format all
```

Formatos: `cursor`, `gemini`, `claude`, `all`.

## HTTP API

### Salud
```
GET /health
GET /api/health
```

### Métricas
```
GET /metrics
```

```bash
curl http://127.0.0.1:3000/metrics
```

Expone los contadores de observabilidad en formato texto de Prometheus `0.0.4` (`llamar_chat_requests_total`, `llamar_http_requests_total`, latencias, enrutado, decisiones `Auto` y coste, entre otros). Es un endpoint **público**: no necesita `X-Debug` ni `X-Admin-Token`, y **no contiene secretos** — ninguna clave, token ni dato sensible se publica en las métricas. Los mismos contadores están disponibles en el campo `observability` de `/health` y en la TUI.

### Modelos
```
GET /models
GET /api/models
```

### Chat
```
POST /chat
POST /api/chat
POST /v1/chat/completions
```

**Headers:**
- `X-Project` — selecciona el proyecto
- `X-Agent` — selecciona un agente dentro del proyecto (si se omite y el router está desactivado, que es el caso por defecto, usa el agente general del proyecto; con `ROUTER_ENABLED=true` el router elige, ver más abajo)
- `X-Debug` — si se envía, incluye `debug_prompt` y `debug_route` en la respuesta

**Validación estricta:** si se envía `X-Project`, Llama-R **requiere** que exista un agente válido dentro de ese proyecto. Si no se encuentra, responde con `400 Bad Request`. Ese agente puede salir del router (abajo), pero nunca sale del **conjunto de candidatos**: el enrutado no puede alcanzar el agente de *otro* proyecto, y sí puede alcanzar un **agente global**, que se despacha sin proyecto y por tanto sin el contexto, las reglas, las fuentes RAG ni las herramientas de ese proyecto — el único límite es que nunca cruza a otro proyecto. Una petición que nombra un proyecto nunca cae en un modelo sin agente.

### Enrutamiento de agentes (opt-in)

Con `ROUTER_ENABLED=true`, una petición con `X-Project` y sin `X-Agent` se enruta hacia el agente más adecuado de ese proyecto (más los agentes globales) mediante un modelo de decisión **Liquid AI d1**, en lugar de ir siempre al agente general. d1 no genera tokens: evalúa preguntas tipadas en un solo forward pass y devuelve probabilidades calibradas, así que **Ollama no puede servirlo** (sus endpoints son generativos). Necesita un `llama-server` aparte:

```bash
llama-server -hf LiquidAI/d1-3B-GGUF:Q8_0   # sirve POST /v1/systemone en :8080
```

Está **desactivado por defecto**: actualizar Llama-R sin tocar nada más no cambia su comportamiento. Si nada encaja (confianza por debajo de `MIN_CONFIDENCE`, especialización por debajo de `MIN_SPECIALIZED`, o el router no responde), la petición queda en el **agente general del proyecto** (`contextos/projects/<p>/agents/<p>.toml`), nunca en un modelo sin agente — y si el proyecto no tiene agente general no hay dónde degradar: responde `400`, como antes de que existiera el router, y sin llegar a gastar una decisión. Un router caído no impide el arranque: el servidor levanta igual, `/api/health` reporta `router` (donde `healthy` es una comprobación TCP del origen configurado, no una petición a `/v1/systemone`) y las peticiones degradan al agente general (o responden `400` si el proyecto carece de uno).

Para probarlo de extremo a extremo (asumiendo el puerto por defecto de llama-r, `3000`):

```bash
# 1. levantar el modelo de decisión
llama-server -hf LiquidAI/d1-3B-GGUF:Q8_0

# 2. activar el router en .env y arrancar
#    ROUTER_ENABLED=true
cargo run

# 3. decidir sin ejecutar el chat (calibra MIN_CONFIDENCE)
curl -s http://127.0.0.1:3000/api/route \
  -H 'Content-Type: application/json' -H 'X-Debug: true' \
  -H 'X-Admin-Token: <token>' \
  -d '{"project_id":"mi-proyecto","prompt":"hazme un plan de comidas"}'

# 4. chatear sin X-Agent: el router elige el agente
curl -s http://127.0.0.1:3000/api/chat \
  -H 'Content-Type: application/json' -H 'X-Project: mi-proyecto' \
  -d '{"messages":[{"role":"user","content":"hazme un plan de comidas"}]}'
```

Variables: `ROUTER_ENABLED` (`false`), `ROUTER_URL` (`http://127.0.0.1:8080`), `ROUTER_MODEL` (`d1-3b`), `ROUTER_TIMEOUT_MS` (`3000`), `MIN_CONFIDENCE` (`0.60`), `MIN_SPECIALIZED` (`0.50`). Un `router.toml` opcional en el directorio base las sobreescribe (y es el único sitio donde poner la lista `exclude`).

gRPC hereda el enrutado (mismos `x-project`/`x-agent` en metadatos, mismo dispatch). La TUI no enruta: ahí eliges el agente a propósito.

El enrutado ocurre **por petición, no por conversación**: cada turno vuelve a decidir, así que dentro de un mismo hilo el agente puede cambiar entre un mensaje y el siguiente. No hay estado de afinidad.

Para curar la calidad del enrutado, cada agente acepta un `[routing]` opcional en su TOML con un único campo `hint`: una frase curada que **sustituye** la descripción que d1 puntúa para ese candidato (`derive_criteria_text`). Es la única palanca curada que hay, y `POST /api/agents` / `PUT /api/agents/:id` no la tocan: `build_agent_toml` reescribe el manifiesto entero, así que un `PUT` borra el `hint` que estuviera escrito a mano.

### Agentes CRUD
```
GET    /api/agents
POST   /api/agents
GET    /api/agents/:id
PUT    /api/agents/:id
DELETE /api/agents/:id
```

### Scope de un agente
```
GET /api/agents/:id/scope
```
Devuelve el `AgentScope` inmutable que se aplica por request.

### Contextos CRUD
```
GET    /api/contexts
POST   /api/contexts
GET    /api/contexts/:id
PUT    /api/contexts/:id
DELETE /api/contexts/:id
POST   /api/contexts/:id/analyze
```

### MCP
```
GET  /api/mcp
POST /api/mcp
```
Endpoint JSON-RPC para interactuar con el gateway como servidor MCP.

### Webhooks (canales firmados)
```
POST /api/webhooks/:channel
```
Un canal firmado ejecuta un turno de agente. La firma se verifica sobre el cuerpo **crudo antes de parsear** el JSON: firma ausente, malformada o errónea, o canal sin secreto, dan un `401` genérico. Canal desconocido o desactivado da `404`. `mode = "sync"` responde `200` con el `ChatResponse`; `mode = "async"` (default) responde `202 {"execution_id": "..."}`, observable en `GET /api/executions/:id`. El scope y el gate de debug salen de `channels.toml`, nunca de la petición. Ver "Canales webhook (Fase 8)".

## Historial de conversaciones

Cada turno de agente se persiste en SQLite cuando el agente tiene `[memory] persist_history = true` (feature `history`, activa por defecto; DB en `{LLAMA_R_DIR}/data/history.db`, ignorada por git). El esquema es aditivo (`IF NOT EXISTS`): un `history.db` antiguo se abre sin migración destructiva. Los ocho huecos conocidos del plan están cerrados (siete en código, el octavo por decisión documentada + test); ver [`PHASE_5_HISTORY_PLAN.md`](./PHASE_5_HISTORY_PLAN.md).

- **Reanudar un hilo:** envía `X-Conversation-Id` (en gRPC, metadato con el mismo nombre). Si se persiste una conversación y no envías id, el servidor genera un UUID.
- **Id de vuelta:** la respuesta JSON trae `conversation_id: Option<String>` (omitido si es `None`) **y** el header `X-Conversation-Id`, tanto en la respuesta JSON como en la SSE, para que el cliente pueda reenviarlo. Si la persistencia está desactivada o no se escribió nada, no hay campo ni header: todas las escrituras —y el id emitido— están condicionadas al id que `append_user` devolvió de verdad, así que un store caído nunca devuelve un id fantasma. Un cliente antiguo que no envía el campo sigue funcionando.
- **Identidad:** el `agent_id` guardado es el id de archivo del agente (`agent.id`, sin `.toml`), nunca el `name` de display: dos agentes con el mismo nombre visible mantienen historiales separados.
- **Resúmenes:** con `index_summaries = true` se indexa en `summary_collection` (o `agent.memory_source_id()` si la política RAG de escritura lo permite). El resumen se aplica a la conversación **del turno**, no a la más reciente del agente, y es idempotente por rango de turnos. El summarizer es inyectable (`trait Summarizer` en `src/ports/history.rs`) y el boot elige una estrategia real para **los seis providers** mediante `build_summarizer` (`src/runtime.rs`): Ollama usa `OllamaSummarizer` (wire `/api/chat`), OpenRouter / `openai-compatible` / Anthropic / Gemini usan `ChatCompletionsSummarizer` (clave Bearer en header, nunca en URL ni logs), y OpenCode usa `OpenCodeSummarizer` (`/api/experimental/generate/text`, password Basic opcional). El **mismo** `Arc<dyn Summarizer>` alimenta este store y el compactor de contexto: un solo cliente HTTP por provider, creado en el boot. Sin URL/key usable se instala el `NoopSummarizer` silencioso con un aviso que nombra solo la variable, y las conversaciones siguen persistiendo sin resúmenes; los tests cubren el camino feliz con un `FakeSummarizer`, sin red. Ver "Contexto activo y selección de modelo" para el cableo y los avisos de operación.
- **Eventos de herramientas:** se guardan el nombre de la herramienta y su éxito (`append_tool_event(conversation_id, tool_name, success)`), **sin argumentos ni salida**: ningún secreto puede llegar a esa tabla. Un sink aditivo en `AgentRunRequest` alimenta por igual los bridges MCP y los locales, en streaming y sin streaming.
- **Retención:** `HISTORY_RETENTION_DAYS` (default 90), purge al arrancar y cada 24 h. El `retention_days` de `[memory]` de cada agente gana sobre el valor global para sus propias conversaciones; el mapa se reconstruye desde `AgentRegistry::list()` en el boot y en cada tick diario, así que un hot reload se aplica sin reiniciar. **`HISTORY_RETENTION_DAYS=0` es un kill-switch absoluto:** desactiva el purge por completo y ningún valor por agente puede reactivarlo.
- **Concurrencia (gap 8 — decisión, no refactor):** se mantiene un `Mutex<Connection>` con WAL; no hay `tokio::sync::Mutex`, ni `spawn_blocking` por query, ni refactor async: solo se reconsiderarían con contención **medida**. Un test de dos appends concurrentes comprueba que ambas filas sobreviven.
- **Best-effort:** fallos de persistencia, resumen o purge avisan con un warning y nunca rompen el chat. Queda diferido: reinyectar en el prompt los resúmenes guardados por el store de conversaciones (el bloque `## Conversation memory` del contexto activo es un resumen de compaction distinto, ver la sección siguiente), el crate OpenTelemetry (hay un exporter OTLP/HTTP crudo opt-in, ver abajo) y auth en los endpoints de historial (siguen públicos; el token admin cubre aprobaciones/ejecuciones/RAG/route/agentes/contextos). La compaction por presupuesto y el model routing por complejidad ya están implementados.

API de administración:

```
GET    /api/conversations
GET    /api/conversations/:id
GET    /api/conversations/:id/messages
POST   /api/conversations/:id/export
DELETE /api/conversations/:id
```

## Contexto activo y selección de modelo (Fase 5)

Dos puertos aditivos —`ContextCompactor` (`src/ports/compaction.rs`) y `ModelSelector` (`src/ports/model_selection.rs`)— más `ActiveContext` (`src/context/active.rs`) y el `ContextAssembler` (`src/services/context_assembler.rs`) permiten que un turno respete la ventana real del modelo y elija modelo por complejidad, ambos con degradación segura al comportamiento anterior. Ningún puerto sagrado (`AgentEngine`, `LLMProvider`, `DecisionRouter`) cambió.

- **`model_candidates`** — campo del TOML del agente (`Vec<String>`, `#[serde(default)]`, vacío = sin cambio). Vacío, el turno corre en `model` (o `DEFAULT_MODEL`) igual que antes y no se consulta al selector; `PUT /api/agents/:id` lo preserva como `routing`/`description`/`subagents`.
- **`models.toml` opcional** en el directorio base mapea `model = <entero de rango>` (`relative_cost: u32`, p. ej. `1` barato, `3` caro). El rango es **ordinal**, solo desempata dentro del nivel que eligió el router (`simple` → rango más bajo, `complex` → más alto; en empate gana el primer candidato declarado). Un candidato ausente de la tabla vale `1`. Sin archivo no hay tabla; un archivo ilegible o mal formado avisa nombrando la ruta y deja la tabla vacía, sin frenar el boot. El archivo solo contiene nombres de modelo y enteros: ni credenciales ni precios.
- **Presupuesto** — `min(capabilities.max_context_tokens, agent.max_context_tokens)`: un lado ausente (o cero) no limita, y si faltan ambos **no hay compaction** (el historial original pasa intacto y ni se consulta al compactor). `reserved_tokens` (`DEFAULT_RESERVED_TOKENS`) se reserva para esquemas de herramientas y documentos RAG recuperados. Solo se compacta el **historial de conversación**: se resumen los turnos más antiguos y se conserva el sufijo más reciente; el turno más reciente nunca se descarta.
- **Colocación de la memoria** — el resumen del compactor se añade al system prompt como sección `## Conversation memory` **después** de las secciones de instrucciones y **antes** del historial que recibe el engine. Las secciones de instrucciones/reglas/skills/contexto/RAG se construyen una sola vez y se reinyectan byte-for-byte, así que son idénticas con o sin compaction; un prompt ya optimizado aportado por el llamador también se conserva literalmente.
- **Degradación** — el selector usa el mismo `Arc<dyn DecisionRouter>` inyectado que el routing de agentes y `Auto` (sin crear un segundo router), configurado con `ROUTER_URL`/`router.toml`. Con el routing apagado o sin router disponible se instala `CurrentModelSelector` y se mantiene el modelo actual; un error del router, `Noul`, respuesta ausente, confianza no finita o baja, o una elección que nombra un modelo no declarado también conservan el modelo actual con un aviso. Un fallo del compactor avisa y vuelve al prompt y al historial originales: el chat nunca falla. El boot ya no fija el compactor a no-op: instala `SummarizingCompactor` con un `Summarizer` apropiado para el provider, elegido por `build_summarizer`. `NoopCompactor` se instala **solo** cuando no hay URL/key usable, porque aparear el compactor real con un summarizer silencioso descartaría los turnos antiguos sin devolver memoria. El compactor vive en `AppState`, fuera del `ProviderStack` que se intercambia: un Apply de Settings reconstruye el engine desde ese **mismo handle** (no cambia de tipo al aplicar), y el boot comparte el único `Arc<dyn Summarizer>` con el store de historial.

- **Avisos de operación del summarizer** — todas las estrategias reales comparten el mismo prompt de resumen y un cliente HTTP con timeout de 30 s; las claves viajan solo en headers `Authorization` y los avisos nombran únicamente la variable, nunca su valor. El summarizer de OpenRouter hace `trim()` de la clave mientras el adapter de chat la usa literalmente: una clave con espacios accidentales alrededor sigue resumiendo pero puede fallar en el chat, así que configura claves sin espacios. El password de OpenCode es opcional: un servidor local sin password sigue teniendo summarizer real; solo un `OPENCODE_URL` vacío degrada. **Anthropic y Gemini no tienen `base_url` en sus perfiles built-in** (sus clientes nativos de chat usan sus propios endpoints), así que por defecto degradan al summarizer no-op y por tanto a `NoopCompactor` (sin `## Conversation memory`). Para activar la compaction, define `[providers.anthropic]` / `[providers.gemini]` en `providers.toml` apuntando a un **gateway compatible con chat-completions**; apuntar a los endpoints nativos `/v1/messages` o `:generateContent` no es compatible y falla cerrado: el chat continúa y el resumen se omite con un aviso.

**No se afirma:** ni precios reales de providers —`relative_cost` es un ordinal y nunca se convierte en `cost_usd`, y nada se escribe en `ModelCapabilities`—, ni dependencia OpenTelemetry (el exporter es OTLP/HTTP JSON crudo sobre `reqwest`), ni compaction de prompts internos de herramientas o RAG (solo del historial), ni modelo de decisión hardcodeado —la pregunta usa las claves de nivel `simple`/`complex` y el modelo es el que configure el router—, ni cambio en agentes que no declaren `model_candidates`, ni endpoint nuevo. Un hijo delegado (Fase 4) hereda el presupuesto pero ensambla **su propio** `ActiveContext` desde su manifiesto: nunca hereda el contexto del padre, igual que hereda scope cero. La ruta HTTP ya entrega sus instrucciones optimizadas mediante `assemble_with_instructions`, así que las métricas siguen siendo veraces y las herramientas de optimización no se ejecutan dos veces.

Código: `src/context/active.rs`, `src/services/context_assembler.rs`, `src/ports/{compaction,model_selection}.rs`, `src/adapters/context/`, `src/adapters/history/summarizer.rs`, `src/ports/history.rs` (`Summarizer::is_noop`), `src/providers/model_costs.rs`, `src/runtime.rs` (`build_summarizer`, `build_model_selector`, companions), `src/api/agent_api.rs`. Cobertura: `tests/context_pipeline_tests.rs`, `tests/summarizer_wire_tests.rs`, `tests/summarizer_selection_tests.rs`, `tests/compaction_wiring_tests.rs`.

## gRPC API

Puerto `50051`. Proto definido en `proto/llamar.proto`.

```protobuf
service LlamaGateway {
  rpc Chat (ChatRequest) returns (ChatResponse);
  rpc ChatStream (ChatRequest) returns (stream ChatStreamEvent);
  rpc ConnectMcp (stream McpMessage) returns (stream McpMessage);
}
```

Headers: `x-project`, `x-agent` en metadatos gRPC.

## Agentes

### Formato TOML

Los agentes se definen en archivos TOML editables. Campos principales:

```toml
name = "MiAgente"
# Opcional: sobreescribe el PROVIDER global para este agente.
# Acepta un nombre canónico (ollama | openrouter | opencode |
# openai-compatible | anthropic | gemini) o un perfil de providers.toml.
provider = "anthropic"
model = "llama3.2"
system_prompt = "Eres un asistente experto en..."
description = "Descripción para UIs y MCP"
context_project = "mi-proyecto"
rules = ["Usa español siempre"]
skills = ["rust-best-practices"]
mcp_sources = ["filesystem", "fudi"]
tools_override = ["*"]
rag_sources = ["docs", "agent:mi-proyecto/miagente/memory"]
max_tool_calls = 8
max_iterations = 6
timeout_secs = 90

[optimize]
enabled = true
rules = ["compress_code", "minify_json"]

[memory]
persist_history = true
summarize_every_n_turns = 10
retention_days = 90

[observability]
trace = true
log_tool_args = false
```

### Herramientas locales y skills bajo demanda

Dos campos del TOML del agente (ambos listas, opcionales) controlan las herramientas en proceso y las skills que se cargan solo cuando se necesitan:

```toml
skills = ["reviewer-skill"]
# Opt-in: sin esto, ninguna herramienta local se ofrece al modelo (deny-all).
tools_local = ["llama_r__skill"]
# El cuerpo de esta skill NO va en el prompt: el modelo lo pide con
# llama_r__skill({name: "reviewer-skill"}) y recibe el SKILL.md completo.
deferred_skills = ["reviewer-skill"]
```

Las dos herramientas integradas son `llama_r__skill` (`{name}`) y `llama_r__rag_query` (`{query}`, `top_k` opcional). `tools_local` también acepta ids de manifests en `tools/*.toml`. La query RAG está siempre limitada por el `AgentScope` del agente; un `source_id` en el manifest es solo informativo. Las herramientas locales comparten con MCP el presupuesto de llamadas (`max_tool_calls`) y solo existen en la ruta de agente con un modelo capaz de llamar herramientas. No hay variables de entorno nuevas.

### Almacenamiento

- `agents/` — agentes globales
- `contextos/projects/<project_id>/agents/` — agentes por proyecto

### Scope

Cada request resuelve un `AgentScope` inmutable que define el perímetro de seguridad:
- `mcp_sources` — servidores MCP permitidos
- `tools_allow` — `DenyAll`, `AllFromSources`, o `Allowlist` específica
- `local_tools_allow` — igual que `tools_allow`, pero para los ids de `tools_local`; se vuelve a comprobar en cada llamada
- `rag_sources` — fuentes RAG legibles
- `rag_write` — política de escritura (`none`, `own_memory_only`, `listed`)
- `max_tool_calls`, `max_iterations`, `timeout_secs`

## Servidores MCP

Configuración en `mcp-servers/*.toml`:

```toml
id = "fudi"
transport = "http"
url = "http://127.0.0.1:4100/mcp"
auth_env = "FUDI_MCP_TOKEN"
timeout_secs = 30
tool_namespace = "fudi"
enabled = true
```

Solo servidores `enabled = true` se cargan. Si un agente referencia un servidor desconocido en `mcp_sources`, se emite una advertencia pero no se bloquea la carga.

Las conexiones tipadas añaden tres campos opcionales (todos con default, los perfiles v1 cargan igual): `headers = [["X-Tenant", "$MI_VAR"]]` (un valor `$VAR` se resuelve del entorno al construir el cliente), `auth = "bearer" | "basic" | "none"` (por defecto `bearer`, `none` hay que pedirlo explícitamente) y `auth_user_env` (nombre de la variable con el usuario Basic; la contraseña sigue viniendo de `auth_env`). Los secretos nunca van en el TOML: solo nombres de variables. Las cabeceras de la conexión se aplican **después** de las del modelo/request y la autenticación se inserta la última, así que un `Authorization` falso enviado como argumento no puede sobrescribir la credencial configurada. `transport = "openapi"` toma en `url` la ruta a un spec JSON OpenAPI 3.x / Swagger 2.0 y expone cada operación como tool; un spec inválido se advierte y se omite sin detener el arranque.

### Aprobación humana (HITL)

Un agente puede exigir aprobación antes de ejecutar ciertas herramientas con una política declarativa en su TOML:

```toml
mcp_sources = ["fudi"]
tools_override = ["*"]

[policy]
connections_allow = ["fudi"]

[[policy.approvals]]
tool_pattern = "fudi__drop*"   # server__tool para MCP; id pelado para una tool local
policy = "always"              # never | once | always | auto
```

`tool_pattern` es un glob simple (`*` = cualquier secuencia, resto literal, toda la cadena) y **gana la primera regla que coincide**; sin coincidencia el default es `never` (sin aprobación). `once` y `always` siguen el camino humano (no hay supresión de `once` tras el primer uso, así que hoy `once` ≡ `always`). `auto` sí consulta el router de decisión (ver "Decisión `Auto`, trazas y coste" más abajo) y puede aprobar sin humano solo con un `yes` de alta confianza; sin router degrada a humano. Sin reglas, ninguna tool pide aprobación.

La tool se bloquea hasta que alguien decide, con un `approval_id` (UUID) ligado al `args_hash` (SHA-256 de los argumentos canónicos). El presupuesto de llamadas **no se carga mientras espera**: una negativa, expiración o cambio de argumentos no gasta nada. El timeout por defecto es 120 s. En un chat con streaming llega un evento SSE con el campo `approval` (`id`, `tool_name`, `args_hash`; sin argumentos en crudo). El stream gRPC lleva los mismos dos campos aditivos que SSE — `approval` (con `task_id`/`depth` opcionales para turnos delegados) y `tool` (`kind`/`tool_name`/`args_hash`/`is_error`) — sin argumentos ni vistas previas de salida en crudo. Sin streaming, el cliente descubre el pendiente sondeando `GET /api/approvals` y decide con `POST /api/approvals/:id`:

```bash
# Ver pendientes (argumentos con campos sensibles redactados como [REDACTED]):
curl -s http://127.0.0.1:3000/api/approvals -H 'X-Admin-Token: <token>'

# Aprobar o denegar; args_hash es opcional (si se omite se usa el hash guardado):
curl -s -X POST http://127.0.0.1:3000/api/approvals/<approval_id> \
  -H 'X-Admin-Token: <token>' \
  -H 'Content-Type: application/json' \
  -d '{"approve": true}'
```

Errores: `400` hash malo/distinto, `404` id desconocido, `409` ya decidida, `410` expirada. Estos endpoints **no** requieren `X-Debug` (son la respuesta humana, no una vista de administración), pero sí el token admin compartido `X-Admin-Token` como el resto del grupo; siguen sin identidad de usuario (el token es un secreto compartido, no un principal). Las aprobaciones **sí sobreviven a un reinicio**: los pendientes y las decisiones se guardan en el store de ejecución y se rehidratan al arrancar; una decisión tomada mientras no había petición viva es consumida por el reintento. También sobreviven a un cambio de provider en caliente desde la TUI.

### Ejecución durable: observación y reintento

Cada turno de agente queda registrado en `data/history.db` (tablas `executions`, `execution_events`, `tool_operations`, `pending_approvals`, añadidas de forma no destructiva). Tras un arranque, los turnos que quedaron `Running`/`WaitingApproval` por un crash se marcan `Interrupted` y pueden reintentarse. Los tres endpoints están tras `X-Debug: true` (como RAG/Router):

```bash
# Ver los turnos recientes (de más reciente a más antiguo; ?limit, default 50, max 200):
curl -s http://127.0.0.1:3000/api/executions \
  -H 'X-Debug: true' -H 'X-Admin-Token: <token>'

# Ver un turno y su log de eventos (argumentos/secrets redactados):
curl -s http://127.0.0.1:3000/api/executions/<execution_id> \
  -H 'X-Debug: true' -H 'X-Admin-Token: <token>'

# Reintentar un turno Interrupted o Failed:
curl -s -X POST http://127.0.0.1:3000/api/executions/<execution_id>/retry \
  -H 'X-Debug: true' -H 'X-Admin-Token: <token>'
```

El reintento reutiliza el **mismo id** de ejecución: las herramientas ya completadas se replayan desde la caché (`operation_id = <execution_id>:<tool>:<args_hash_hex>`) en lugar de repetir sus efectos. Es recuperación **a nivel de turno**, no replay paso a paso: se re-ejecuta el turno desde el `user_message` guardado (`stream: false`, sin recargar el historial de conversación previo) y el modelo puede generar una respuesta distinta. El reintento da `200` si el turno se re-ejecuta, `400` id malformado, `403` sin `X-Debug` o fila legacy sin `user_message`, `404` id desconocido y `409` si el turno no está en estado reintentable. Sin store (build `--no-default-features` o fallo al abrirlo) la ejecución degrada a pass-through y el chat sigue funcionando. Queda diferido: backend Postgres/async, identidad de usuario en los endpoints (hoy hay un token compartido, no un principal), supresión de `once` tras el primer uso, dependencia OpenTelemetry (existe un exporter OTLP/HTTP crudo) y replay exacto de pasos. El modelo de decisión `Auto` ya está implementado (ver sección siguiente).

### Decisión `Auto`, trazas y coste (Fase 6)

**Aprobación `Auto`.** Con una regla `policy = "auto"`, el gate consulta el `AutoDecider` (`RouterAutoDecider`, montado sobre el mismo `DecisionRouter` d1 del routing de agentes) **solo si `ROUTER_ENABLED=true`**. Aprueba sin humano únicamente cuando el modelo responde `Choice{key:"yes"}` con `confidence >= umbral`; un `no`, baja confianza, `Noul`, respuesta inusable, error del router o ausencia de decider degradan al camino humano (nunca se aprueba automáticamente sin evidencia afirmativa). La pregunta al modelo lleva el nombre de la tool, el `args_hash` canónico y los argumentos **redactados** — los argumentos se redactan con `redact_secrets` antes de salir del proceso, así que ningún secreto en crudo llega al modelo. En la práctica, como el d1 casi siempre está ausente, `Auto` ≈ `always`: el seam está implementado y probado con un router falso. El `[policy]` / `[[policy.approvals]]` acepta un campo opcional `auto_threshold` (`Option<f32>`): si está en `(0.0, 1.0]` sustituye el umbral por defecto del decider (`MIN_CONFIDENCE` del router, 0.60); un valor fuera de rango o no finito se ignora con un aviso que nombra solo el número y la tool, nunca argumentos. La función pura `PermissionEngine` sigue tratando `Auto` como `Approve`; el cambio está en el gate.

**Trazas por turno.** Cada turno finalizado (éxito, fallback o error, con o sin streaming) emite un evento `TraceRecorded` —best-effort: un fallo al persistir es un aviso, nunca un chat fallido— consultable en `GET /api/executions/<execution_id>`. Su payload, redactado con `redact_trace_payload` (conserva los contadores numéricos y redacta secretos), contiene: `trace_id` (= `execution_id`), `agent_qualified_id`, `provider`, `model`, `input_tokens`, `output_tokens`, `cost_usd` y `latency_ms`; en turnos delegados añade `parent_execution_id`, `root_execution_id` y `depth` opcionales (ver "Subagent delegation").

**Coste y observabilidad.** `cost_usd` sale de `Usage::from_counts` a partir de los precios de `ModelCapabilities`; hoy el catálogo no tiene precios, así que el coste es `null` en la práctica — el cálculo existe y se prueba con precios inyectados, pero no se inventa ningún coste. El snapshot de observabilidad (`campo observability` de `/health`, `ObservabilitySnapshot`) expone además `auto_approved`, `auto_denied` y `auto_unavailable` (contadores de las decisiones `Auto` en el gate —un decider ausente cuenta como `unavailable`; `never`/`always`/`once` no los tocan—) y `cost_usd_total_micros` (coste real acumulado en microdólares enteros, saturante; sin entrada mientras los precios sean `None`).

**No se afirma:** ni dependencia/integración con OpenTelemetry (el exporter es OTLP/HTTP JSON crudo sobre `reqwest`, sin crate nuevo; con `OTEL_ENABLED=false` no envía nada), ni supresión de "primera vez" para `once`, ni coste real mientras el catálogo no tenga precios. El runner de evals tampoco incluye un check de aprobación `Auto` (`judge` solo califica respuestas).

## Canales webhook (Fase 8)

Un webhook entrante firmado se convierte en un turno de agente. El único endpoint nuevo es `POST /api/webhooks/:channel`. Los canales se definen en un `channels.toml` opcional del directorio base, que se carga **por petición** (como agentes y contextos): un cambio se aplica sin reiniciar. Sin archivo no hay canales y no hay aviso; un archivo ilegible o malformado produce un aviso con la ruta y un conjunto vacío — nunca bloquea el arranque.

```toml
[[channels]]
id = "github"
agent_id = "billing"
project_id = "proj"          # opcional
mode = "async"               # o "sync"; default async
template = "{{action}} de {{sender.login}}"
signature_header = "X-Signature"
auth_env = "WEBHOOK_SECRET"  # NOMBRE de la variable, nunca el secreto
enabled = true
```

- `id` — nombre en la ruta de la URL; un canal desconocido o desactivado da `404`.
- `agent_id` — agente destino obligatorio; `project_id` opcional lo limita a un proyecto. Ambos salen de la config, nunca de los headers del emisor. El turno se despacha con un `AgentSelection` solo de config, con `debug` **siempre `false`**: el llamador no puede ampliar el scope ni activar `debug_prompt`/`debug_route`.
- `mode` — `"sync"` ejecuta el turno en línea y responde `200` con el `ChatResponse`; `"async"` (default) crea el id **antes** de lanzar la tarea en proceso, responde `202 {"execution_id": "<uuid>"}` y el turno se observa en `GET /api/executions/:id` (el log de ejecución de Fase 7).
- `template` — plantilla de marcadores opcional (abajo); ausente o en blanco, el mensaje es el cuerpo crudo.
- `signature_header` — header con la firma; por defecto `X-Signature`.
- `auth_env` — el **NOMBRE** de la variable de entorno con el secreto compartido, nunca el secreto. `ChannelConfig` no tiene campo de secreto, así que se puede hacer `Debug` y loguear sin riesgo; el valor se resuelve del entorno al atender la petición.
- `enabled` — `bool`, default `true`.

**Firma antes de parsear.** El puerto `ChannelSignatureVerifier` tiene un adaptador, `HmacSha256Verifier` (`src/adapters/channel/hmac.rs`): HMAC-SHA256 sobre los **bytes crudos** de la petición — sin parsear, recortar ni re-serializar — que acepta `sha256=<hex>` y `<hex>` suelto, decodifica 32 bytes y compara en tiempo constante. Falla cerrado: secreto ausente o vacío, header ausente o malformado, o discrepancia, todos dan el mismo `401 invalid signature`, genérico. Ningún cuerpo, firma ni secreto se refleja en una respuesta o un log; el único aviso nombra la variable, nunca su valor. El puerto es puro (cuerpo, header, secreto resuelto → `bool`) y se comprueba con vectores RFC 4231.

**Plantillas de marcadores, sin Liquid.** Una plantilla es texto plano con marcadores `{{ruta.al.campo}}` que se sustituyen por escalares del JSON entrante: strings tal cual, números y booleanos como texto. **No hay motor Liquid, lenguaje de expresiones, escape HTML ni dependencia nueva.** Una ruta ausente o desconocida, un valor no escalar (objeto/array/null) o un `{{` sin cerrar dan un `400` nombrado que menciona solo la ruta de la plantilla — nunca el cuerpo —, así que un secreto en el payload no puede devolverse en el error. `message_from_template` mantiene en un solo sitio la regla «cuerpo crudo si no hay plantilla», compartida por producción y tests.

**No se afirma:** ni cola de entrega, reintentos ni reaper en segundo plano — `async` es un único turno lanzado en proceso; ni identidad de usuario ni auth en el endpoint — la firma **es** la auth; ni esquemas de firma específicos de proveedor (un solo HMAC-SHA256, con header configurable); ni secreto o cuerpo en logs/errores; ni dependencia nueva (HMAC, decodificación hex y comparación en tiempo constante se apoyan en `sha2`, ya presente; no se añaden `hmac`/`hex`/`subtle`); ni cambio en puertos sagrados; ni otro endpoint además de `POST /api/webhooks/:channel`. Los agentes remotos y el sandbox siguen sin hacerse (ver "Subagentes (Fase 4)").

## Subagentes (Fase 4)

Un agente puede delegar un turno a otros agentes solo con TOML. La tabla `[subagents]` deja de ser un stub inerte y pasa a ser una política tipada en `AgentConfig` (`Option<SubagentsConfig>`):

```toml
[subagents]
allowed = ["especialista-a", "especialista-b"]  # ids del mismo proyecto; vacío = deny-all
allow_global = false                            # permite delegar a agentes globales de agents/ (default false)
max_depth = 2                                   # niveles máximos desde el coordinador raíz; default 1, rango 1..=8
```

- **`max_depth`** es el número de niveles de delegación desde el raíz. Si se omite vale `1`; el rango válido es `1..=8` (techo duro `8`). Un valor fuera de rango se ignora con un aviso que nombra `subagents.max_depth` y el valor, y cae a `1` — nunca se recorta en silencio. El **límite efectivo** es el mínimo de `max_depth` a lo largo de toda la cadena (raíz más cada ancestro), con techo `8`: cada salto lo endurece con el límite del agente destino, así que ningún hijo puede ampliar su propio permiso. Todos los campos son `#[serde(default)]`, así que un manifiesto v1 que declara `[subagents]` sigue cargando, y la tabla **sí se serializa** en `PUT /api/agents/:id` para preservar allowlist y profundidad.
- **La herramienta** `llama_r__delegate` tiene esquema `{message (requerido), agent_id?, task_id?}`. `message` debe ser una cadena no vacía; `agent_id` y `task_id`, si aparecen, también. Un `agent_id` explícito se valida contra la allowlist; sin él se usa el primer id de `allowed` como destino, y una allowlist vacía es un rechazo nombrado. Es una herramienta local, así que el agente debe poner `llama_r__delegate` en su `tools_local` para usarla — las herramientas locales son deny-all por defecto.
- **Herencia cero.** El hijo se resuelve solo por su id, validado contra la allowlist y el mismo proyecto (o `agents/` si `allow_global = true`). No recibe el contexto de proyecto, skills, fuentes RAG ni tools del padre: su propio manifiesto y `ScopeBuilder` producen un scope `DenyAll`. Un id no permitido es un `ToolError` nombrado, nunca una delegación.
- **Profundidad y ciclos.** Un salto con `depth > effective_max_depth`, o cuyo id cualificado ya está en la cadena de ancestros, se rechaza con un `ToolError` nombrado y cero turnos hijos ejecutados.
- **Un solo presupuesto.** Todos los turnos del árbol cargan del mismo `Arc<dyn ToolBudget>` (un `Arc<Mutex<ToolCallBudget>>` tras el trait object) que lleva el `DelegateRequest`: hay una sola asignación para todo el árbol, sin importar la profundidad. Agotarlo da un `ToolError` nombrado que sube por la cadena; la carga sigue ocurriendo solo tras `authorize → Ok`.
- **Aprobaciones.** El hijo usa el mismo `ApprovalGate` global. Desde cualquier nivel, un evento `ApprovalRequired` se reenvía al SSE del coordinador raíz con `task_id` y `depth` en el payload de `approval` (campos opcionales y aditivos), preservando el origen a través de niveles más profundos. Sin streaming no hay sender: el pendiente sigue apareciendo en `GET /api/approvals` y se decide con `POST`, sin cambios.
- **Trazas jerárquicas.** Cada turno emite su propio `TraceRecorded`; el payload gana los campos opcionales `parent_execution_id`, `root_execution_id` y `depth`, de modo que `GET /api/executions/<execution_id>` reconstruye el árbol. Se redacta con el mismo `redact_trace_payload` de Fase 6; sin backend nuevo.
- **Aislamiento de fallos.** Un hijo ausente, provider caído, presupuesto agotado o timeout es un `ToolError` nombrado; el bucle del padre continúa y el modelo decide cómo responder, nunca un error del chat raíz. La idempotencia por `operation_id` hace que un reintento reprice los efectos de las tools del hijo en vez de repetirlos, y el deadline recursivo (`restante − 5 s`) hace que el timeout nombrado del hijo llegue antes de que se cancele el bucle del padre.

**No se afirma:** ni agentes remotos ni sandbox (Fase 8), ni *steering* de mensajes a un hijo en vuelo, ni cambios en la TUI, ni dependencia OpenTelemetry (el exporter opt-in es OTLP/HTTP crudo), ni cambio alguno en agentes que no declaren `[subagents]` (v1 intacto), ni un endpoint nuevo.

## Contexto de proyecto

El `ProjectContext` se genera automáticamente via LLM:
1. Escanea la estructura del proyecto
2. Analiza tecnologías, dependencias, patrones
3. Selecciona skills compatibles
4. Genera un contexto markdown enriquecido

Se almacena en `contextos/projects/<project_id>/context/context.json`.

## Skills

Skills se descubren desde múltiples directorios:
- `~/.cursor/skills/`
- `~/.claude/skills/`
- `~/.agents/skills/`
- `./skills/`
- y otros

Cada skill es un subdirectorio con `SKILL.md` (frontmatter YAML: `name`, `description`, `tags`).

El auto-sync (tras `analyze`) selecciona hasta 5 skills relevantes por agente y las escribe en `auto_skills`.

## Optimización de tokens

El `TokenOptimizer` aplica reglas configurables por agente:
- `compress_code` — elimina saltos de línea redundantes en bloques de código
- `minify_json` — compacta objetos JSON

Métricas globales de tokens ahorrados vs procesados disponibles en `/health` y en la TUI.

## Hot reload

El directorio base se observa con `notify`. Los cambios en `agents/`, `contextos/projects/*/agents/` y `mcp-servers/` recargan automáticamente el registro de agentes sin reiniciar el servidor.

## TUI

Terminal UI con tres vistas:
- **Dashboard** — estado del servidor, métricas, agentes cargados
- **Projects** — lista de proyectos, agentes, formulario de edición
- **Agent Form** — edición inline de campos del agente

Navegación con teclas: `Tab` entre paneles, `Enter` para editar.

## Desarrollo

```bash
cargo check
cargo fmt
cargo test --target-dir target-tests
cargo test --test provider_eval_smoke   # smoke multi-provider (fixture, sin red)
cargo test --test evals_tests           # runner declarativo (checks, strict, called_tool, judge)
```

### Evals declarativas

Las evaluaciones se definen en archivos TOML bajo `evals/` (la incluida es
`evals/providers/smoke.eval.toml`). Cada `[[cases]]` lleva `id`, `kind`
(`plain`/`tool`/`stream`/`agent`), `prompt`, `checks` y, para casos `agent`, un
`agent`. Ejemplo mínimo:

```toml
[[cases]]
id = "greeting"
kind = "plain"
prompt = "Saluda brevemente"
expect = "hola"                       # azúcar para un check `contains`
checks = [{ type = "not_contains", text = "adiós" }]

[[cases]]
id = "uses-skill"
kind = "agent"
prompt = "Usa la skill de evaluación"
agent = "eval-agent"
checks = [{ type = "called_tool", name = "llama_r__skill" }]

[[providers]]
name = "ollama"
kind = "ollama"
```

Los cuatro tipos de `checks` son `contains` / `not_contains` (texto),
`called_tool` (nombre cualificado `servidor__herramienta`, o el id simple de una
herramienta local, tomado de eventos reales del agente) y `judge` (califica con
el router de decisión existente; sin router o con baja confianza se marca
`SKIP`, nunca `PASS`). El modo por defecto es `strict`: cualquier `FAIL` falla
la suite, pero un `SKIP` nunca falla. Todo corre contra fixtures locales
grabadas: sin claves reales, red ni modelo de decisión en CI, y los detalles del
informe se redactan antes de imprimirse.

El runner no incluye un check del modelo `Auto` de aprobación (`judge` solo
califica respuestas), aunque la política `Auto` ya existe en el gate. El coste
por turno y las trazas sí existen en el log de ejecución y en el snapshot de
observabilidad; no hay crate OpenTelemetry: el exporter opt-in publica esos campos ya redactados como OTLP/HTTP JSON con `reqwest` (ver la sección siguiente).

```bash
cargo test --test provider_eval_smoke   # consume la suite incluida e imprime la tabla
cargo test --test evals_tests           # pruebas del runner y sus checks
```

## Auth admin, aparcamiento de aprobaciones y telemetría OTLP (cierre Fase 9)

Tres capacidades opt-in comparten una regla: `Config` (`src/config.rs`) es la
única fuente de verdad de sus variables de entorno. `from_env` las parsea,
`save_to_env` las persiste en `.env` (modo `0600`, así el token admin sigue siendo
un secreto), `CONFIG_ENV_VARS` aísla los tests y `AppState.config` es lo que
leen el middleware, el handler de chat, el gate, el reaper y el exporter —
ningún helper vuelve a leer el entorno por petición. Los números mal formados
son un error de configuración que nombra la variable (salvo el intervalo del
reaper: un valor numérico no positivo cae a `5`, como su helper anterior).

1. **Token admin.** Un middleware `from_fn_with_state` (`src/api/admin_auth.rs`)
   envuelve el sub-router admin en `build_router`: `/api/approvals*`,
   `/api/executions*`, `/api/rag/*`, `/api/route`, `/api/contexts*`,
   `/api/agents*`. El cliente envía `X-Admin-Token` y se compara en tiempo
   constante con `state.config.admin_token`. Sin token o vacío, **todas** las
   rutas admin fallan cerradas con un `401` fijo y genérico que nunca repite el
   valor. `X-Debug` se suma, no se sustituye. `/chat`, `/api/chat`,
   `/v1/chat/completions`, `/api/webhooks/:channel`, health, modelos, docs y el
   historial de conversaciones siguen públicos.
2. **Aparcamiento de aprobaciones.** `Config::approval_timeout()` devuelve
   `None` cuando `APPROVAL_PARK` está activo o `APPROVAL_TIMEOUT_SECS` es `0`,
   así una decisión humana pendiente nunca expira (`ApprovalGate::parked()`);
   un turno gated no streaming responde `202 {"execution_id", "approval_id"?}`
   tras `PARK_GRACE_MS`. El estado `WaitingApproval`, el índice `parked_live` y
   el reaper de arranque (intervalo `REAPER_INTERVAL_SECS`) permiten reanudar un
   huérfano ya decidido bajo su mismo id de ejecución, sin doble reanudo.
3. **Telemetría OTLP.** `exporter_from_config` (`src/adapters/telemetry/mod.rs`)
   devuelve un noop salvo que `OTEL_ENABLED` sea truthy. Activado, publica el
   valor `TraceRecorded` ya redactado como un span OTLP/HTTP JSON con el
   `reqwest` existente. Es best-effort: un fallo del colector nunca rompe el
   arranque ni el chat, y no se envía credencial, argumento ni cuerpo alguno.

Las ocho variables y sus defaults están en la tabla de "Variables de entorno".
Cobertura: tests unitarios de `src/config.rs`, `tests/admin_auth_tests.rs`,
`tests/approval_parking_tests.rs` y `tests/otlp_exporter_tests.rs`.

**No se afirma:** ni crate ni SDK OpenTelemetry (OTLP/HTTP JSON crudo sobre
`reqwest`); ni identidad de usuario (*principal*) — un token compartido, no auth
por usuario; ni sandbox ni agentes remotos; ni motor de workflows externo (el
reaper es una tarea en proceso); ni endpoint nuevo más allá del grupo admin ya
existente, ni cambio en los contratos de chat, webhooks, gRPC u
OpenAI-compatible; ni dependencia nueva; ni cambio en puertos sagrados.

## Instalación

Compila e instala el binario `llama-r` en el sistema, permitiendo ejecutarlo desde cualquier directorio:

```bash
cargo install --path .
```

Una vez instalado, puedes usar el comando `llama-r` directamente:

```bash
llama-r                # Inicia servidor HTTP + gRPC + TUI
llama-r init           # Crea agente base del proyecto actual
llama-r init-agent mi-agente
llama-r analyze /ruta/al/proyecto --id mi-proyecto
```

También puedes ejecutarlo sin instalarlo, desde el directorio del repositorio:

```bash
cargo run -- <comando>
```

## Almacenamiento en disco

```
<base_dir>/
├── agents/                     # Agentes globales
│   └── *.toml
├── contextos/
│   └── projects/
│       └── <project_id>/
│           ├── agents/         # Agentes del proyecto
│           │   └── *.toml
│           └── context/        # Contexto generado
│               ├── context.json
│               └── context.md
├── mcp-servers/                # Config MCP
│   └── *.toml
├── providers.toml              # Perfiles de provider (opcional)
├── router.toml                 # Ajustes del router (opcional)
├── channels.toml               # Canales webhook entrantes (opcional)
└── logs/
    └── llama-r.log.YYYY-MM-DD
```

`<base_dir>` se resuelve por orden: `$LLAMA_R_DIR` → directorio del ejecutable → directorio actual.
