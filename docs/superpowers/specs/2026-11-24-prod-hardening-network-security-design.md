# Diseño: hardening operativo de red y seguridad (Fase 10-A)

- **Fecha:** 2026-11-24
- **Estado:** Diseño aprobado en brainstorming; pendiente de revisión del spec
- **Tipo:** Arquitectural — cambia la configuración de red, la superficie pública y el ciclo de vida de los servidores
- **Perfil de deployment:** LAN / localhost, uso personal (sin TLS propio ni rate limiting)
- **Parte:** 1 de 3 del plan de puesta a punto (A = hardening operativo; B = observabilidad/release; C = brechas funcionales)

## 1. Contexto y problema

La auditoría de producción de 2026-11-24 encontró cinco riesgos operativos en llama-r:

1. Los clientes HTTP directos de Ollama, OpenAI-compatible y OpenCode usan `reqwest::Client::new()` sin timeout (`src/providers/{ollama,openai_compatible,opencode}.rs`); el `timeout_ms` de `providers.toml` no se aplica ahí. Un upstream caído cuelga el chat y `/health`.
2. `build_router` usa `CorsLayer::allow_origin(Any)` con `x-admin-token` en la lista de headers permitidos (`src/runtime.rs:337-347`): cualquier origen puede intentar usar el token desde un navegador.
3. Los endpoints de historial (`GET/DELETE /api/conversations*`, `/messages`, `/export`) son públicos (`src/runtime.rs:365-382`), permitiendo lectura y borrado sin autenticar.
4. No hay `DefaultBodyLimit` global: solo webhooks limitan el body a 1 MB (`src/api/webhook_api.rs`).
5. El apagado aborta los tasks sin esperar peticiones en vuelo (`src/main.rs:119-130`, `src/runtime.rs:817-835`): no hay `SIGTERM` ni graceful shutdown, aunque la documentación dice "hasta Ctrl-C/SIGTERM".

Además, el `.env` actual está en modo `0644` aunque la política de secretos exige `0600` (`src/config.rs:442-443` solo lo aplica al guardar desde la app).

Objetivo: dejar el gateway operable en LAN sin cuelgues ni exposición indebida, manteniendo los puertos sagrados (3000 HTTP / 50051 gRPC) y el contrato de las rutas de chat.

## 2. Decisiones

| # | Decisión | Justificación |
|---|---|---|
| D1 | Un módulo `src/net.rs` con **dos fábricas compartidas**: `http_client(timeout)` y `cors_layer(origins)`. | Evita que cada adapter construya un cliente con política distinta; el helper es la única fuente de verdad. |
| D2 | `HTTP_TIMEOUT_MS` (default 30 000) y `CORS_ORIGINS` (default loopback) en `Config`. | `Config` es la única fuente de verdad de env vars (convención Fase 9). `timeout_ms` de `providers.toml` se aplica además en los adaptadores. |
| D3 | CORS por **allowlist**, nunca `Any`. El header `x-admin-token` se mantiene, pero el origen queda restringido. | Perfil LAN: solo orígenes loopback por defecto. Cualquier otro origen debe configurarse explícitamente. |
| D4 | Mover `/api/conversations*` al **grupo admin** (`X-Admin-Token`). | El perfil elegido dice "historial detrás del admin token". **Breaking change documentado**: los clientes actuales deben enviar el token. |
| D5 | `DefaultBodyLimit::max(2 MB)` global; webhooks conservan su límite propio de 1 MB. | 2 MB cubre conversaciones largas; el límite específico de webhooks sigue siendo más estricto. |
| D6 | Validar `messages` no vacío **en la entrada del handler**, no al final de `chat_core`. | Hoy se rechaza tarde (`src/api/chat_core.rs:561`), después de haber gastado resolución de agente. |
| D7 | Graceful shutdown con `ctrl_c` **o** `SIGTERM` (unix), vía `with_graceful_shutdown` en axum y `serve_with_shutdown` en tonic. `main.rs` espera a ambos tasks antes de salir. | En systemd/Docker el proceso recibe SIGTERM, no Ctrl-C. `abort()` corta peticiones en vuelo. |
| D8 | Al arrancar, si `.env` existe y es world-readable en unix, aplicar `chmod 0600` y loguear un warning que nombre solo la ruta. | Corrige el archivo actual sin tocar el código de guardado. Ningún secreto en el log. |

RFC 2119: **MUST**, **SHALL**, **SHOULD**, **MAY**, **MUST NOT**.

## 3. Configuración nueva

```dotenv
# src/config.rs — leídas por Config::from_env, persistidas por save_to_env
HTTP_TIMEOUT_MS=30000      # 0 = rechazado (error de config); malformado = error nombrando la var
CORS_ORIGINS=http://localhost,http://127.0.0.1   # CSV; vacío = solo loopback
```

- `HTTP_TIMEOUT_MS` MUST ser > 0; `0` es error de configuración, no un modo "sin timeout" silencioso.
- `CORS_ORIGINS` acepta orígenes completos (`scheme://host[:port]`). El default cubre todo puerto en localhost/127.0.0.1; una entrada `*` está **prohibida** (se rechaza con error nombrando la variable).
- Ambas variables entran en `CONFIG_ENV_VARS` (guard de test de aislamento) y en `.env.example`.

## 4. Módulo `src/net.rs`

```rust
/// Cliente HTTP con connect_timeout + timeout. Nunca se construye un
/// reqwest::Client::new() en código de producción fuera de aquí.
pub fn http_client(timeout: Duration) -> Result<reqwest::Client, AppError>;

/// CorsLayer con allowlist de orígenes. Nunca devuelve allow_origin(Any).
pub fn cors_layer(origins: &[String]) -> Result<CorsLayer, AppError>;
```

- `http_client` configura `connect_timeout(Duration::from_secs(5))` y `timeout(total)`; ambos derivados de la config. Un `timeout` de 0 se rechaza en `Config`, no aquí.
- `cors_layer` construye `AllowOrigin::list` a partir de los `HeaderValue` parseados; un origen inválido es error de configuración que nombra el valor ofensivo (la URL, no un secreto).
- El módulo expone tests unitarios para ambos helpers (§8).

## 5. Cambios por archivo

| Archivo | Cambio |
|---|---|
| `src/net.rs` | **Nuevo.** Fábricas `http_client` / `cors_layer` + tests. |
| `src/config.rs` | Nuevos campos, parsing con validación, `save_to_env`, `CONFIG_ENV_VARS`, tests. |
| `src/providers/{ollama,openai_compatible,opencode}.rs` | Construir el cliente con `net::http_client(timeout)`; el timeout viene de config + perfil. El `Debug`/logs siguen sin exponer keys. |
| `src/api/health.rs` | El probe de provider usa el mismo cliente con timeout (hoy `src/providers/ollama.rs:83-91` es timeout-less). La salud del router sigue siendo last-known (documentado). |
| `src/runtime.rs` | `build_router`: `cors_layer(...)` en vez de `Any`; mover el grupo de conversaciones bajo `require_admin`; `DefaultBodyLimit`. |
| `src/main.rs` | `shutdown_signal()` compartido; esperar `http_server` y `grpc_server` en vez de `abort()`. |
| `src/lib.rs` | Re-exportar `net` si es público (o `pub(crate)`). |
| `.env.example` | Documentar `HTTP_TIMEOUT_MS` y `CORS_ORIGINS`. |
| `AGENTS.md`, `README.md` | Sección de deployment en LAN: admin token obligatorio para historial, variables nuevas, aviso de breaking change. |

## 6. Flujo de datos (shutdown)

```
ctrl_c()  ─┐
           ├─> shutdown_signal() resuelve
SIGTERM  ──┘
              │
              ├─> axum with_graceful_shutdown: termina peticiones en vuelo
              └─> tonic serve_with_shutdown: termina streams en vuelo
                        │
                        └─> main espera ambos JoinHandle -> exit 0
```

En el TUI, `tui.run()` sigue gobernando el ciclo de vida: al terminar, se dispara la misma señal de parada. `--server-only` espera directamente la señal.

## 7. Compatibilidad y rollback

- **Breaking change:** `/api/conversations*` exige `X-Admin-Token`. Se documenta en `AGENTS.md`/`README.md` y en el changelog de la release.
- `CORS_ORIGINS` con default loopback **puede** romper un cliente web servido desde otro host; se documenta cómo añadir el origen.
- Rollback: revertir los archivos de la tabla §5. No hay migración de datos; `.env` gana dos variables opcionales.
- Los puertos sagrados no cambian; las rutas de chat (`/chat`, `/api/chat`, `/v1/chat/completions`), webhooks y health siguen públicas.

## 8. Testing y verificación

### 8.1 Unit / integración

- `net.rs`: (a) servidor local que duerme > timeout ⇒ `http_client` devuelve error en ~timeout; (b) `cors_layer` con origen permitido ⇒ header presente, con origen ajeno ⇒ ausente; (c) `CORS_ORIGINS=*` ⇒ error de config.
- `config.rs`: `HTTP_TIMEOUT_MS=0` y malformado ⇒ error nombrando la variable; `save_to_env` persiste ambas.
- `runtime.rs`: test de que `GET /api/conversations` sin token ⇒ 401, con token ⇒ 200; payload > 2 MB ⇒ 413.
- `approval_api.rs` y `chat_core.rs`: regresión de que el token sigue siendo obligatorio y los mensajes vacíos se rechazan antes de gastar un turno.

### 8.2 E2E (binario real)

1. Arrancar con `ADMIN_TOKEN=e2e`, `CORS_ORIGINS` por defecto, puerto libre, `LLAMA_R_DIR` aislado.
2. `GET /api/health` ⇒ 200 (con timeout acotado, medir < 5 s con un upstream muerto).
3. `GET /api/conversations` sin token ⇒ 401; con token ⇒ 200.
4. `POST /api/chat` con body > 2 MB ⇒ 413.
5. `curl` con `Origin: http://evil.example` ⇒ sin `Access-Control-Allow-Origin`; con `Origin: http://localhost:3000` ⇒ header presente.
6. Enviar `SIGTERM` al proceso ⇒ termina con código 0 en < 5 s; `GET /api/health` deja de responder.
7. Verificar `stat -c %a .env` ⇒ `600`.

### 8.3 Regresión

`cargo test --target-dir target-tests` (~963 tests), `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`. Ningún servidor de prueba debe quedar vivo antes de la suite (los tests de headless fallan si el puerto está ocupado).

## 9. Riesgos

| Riesgo | Mitigación |
|---|---|
| El timeout de 30 s corte generaciones lentas en CPU | Configurable por `HTTP_TIMEOUT_MS`; el operador puede subirlo. Los modelos cloud responden en < 10 s. |
| Proteger historial rompa clientes actuales | Documentado como breaking change; el token ya existe y es el mismo que usan las demás rutas admin. |
| `with_graceful_shutdown` deje el proceso colgado si un cliente no cierra | El shutdown espera a las peticiones en vuelo; se documenta y se mide en el E2E (< 5 s). |
| Un origen legítimo quede fuera | `CORS_ORIGINS` CSV; `*` prohibido a propósito para no reintroducir el riesgo. |

## 10. Fuera de alcance

`/metrics`, cargo-deny, coverage, release/Docker, LICENSE/CHANGELOG (sub-proyecto B). Compactación real, step-replay, SSE completo y embeddings para OpenCode/Anthropic (sub-proyecto C). TLS propio, rate limiting y multi-usuario (perfil LAN no los requiere).

## 11. Plan de implementación (siguiente paso)

Tras la revisión de este spec, invocar `writing-plans` para desglosar:

1. `Config`: nuevas variables + validación + `.env.example`.
2. `src/net.rs`: fábricas + tests.
3. Adapters de proveedor y health: aplicar timeout.
4. `runtime.rs`: CORS, body limit, grupo admin de conversaciones.
5. `main.rs` + `runtime.rs`: graceful shutdown con SIGTERM.
6. `.env` 0600 en boot + corrección del archivo actual.
7. Docs (`AGENTS.md`, `README.md`) + E2E + suite completa.

Criterio de aceptación: el E2E del §8.2 pasa completo y la suite de 963 tests sigue verde.
