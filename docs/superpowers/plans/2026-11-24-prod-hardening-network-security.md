# Hardening operativo de red y seguridad (Fase 10-A) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Dejar llama-r operable en LAN sin cuelgues de red ni exposición indebida de las rutas de historial.

**Architecture:** Un módulo `src/net.rs` concentra la construcción de clientes HTTP (con timeout) y de la capa CORS (con allowlist); `Config` es la única fuente de las dos variables nuevas; el router mueve el historial al grupo admin y añade body limit; `main.rs` cambia `abort()` por graceful shutdown con `ctrl_c`/`SIGTERM`.

**Tech Stack:** Rust 1.97, axum 0.7 (tower-http CorsLayer, DefaultBodyLimit), reqwest 0.13, tonic 0.14, tokio, `dotenvy`, `sysinfo`/libc para permisos unix.

**Spec:** `docs/superpowers/specs/2026-11-24-prod-hardening-network-security-design.md`

## Global Constraints

- `HTTP_TIMEOUT_MS` default `30000`, MUST > 0; `0` o malformado = `AppError::Config` nombrando la variable.
- `CORS_ORIGINS` default `http://localhost,http://127.0.0.1`; valor `*` prohibido = error de config nombrando la variable.
- Ambas variables en `CONFIG_ENV_VARS` y en `.env.example`.
- Body limit global `2 MB` (`2 * 1024 * 1024`); webhooks conservan su límite de 1 MB.
- `/api/conversations*` (GET/DELETE, `/messages`, `/export`) bajo `X-Admin-Token`. Rutas de chat, webhooks, health, models y MCP siguen públicas.
- Puertos sagrados sin cambio: HTTP 3000, gRPC 50051.
- `.env` MUST terminar en modo `0600` tras boot en unix.
- Ningún secreto en logs, URLs, health ni mensajes de error. CERO `allow_origin(Any)` en código de producción.
- `cargo test --target-dir target-tests` MUST pasar (963 tests) antes de cada commit de tarea.

## Review Focus

- Origin `*` o malformado en `CORS_ORIGINS` → error de config, no `allow_origin(Any)` silencioso. Test en Task 1.
- Upstream que duerme más que `HTTP_TIMEOUT_MS` → `http_client` devuelve error en ~timeout, incluido `/health`. Tests en Tasks 2 y 3.
- Cliente web sin header `Origin` o con origen ajeno → sin `Access-Control-Allow-Origin`; origen loopback → header presente. Test en Task 4.
- Request sin `X-Admin-Token` a `/api/conversations` → 401 antes de tocar el store; token correcto → 200. Test en Task 4.
- Payload > 2 MB → 413 sin pasar al handler. Test en Task 4.
- `SIGTERM` durante una petición en vuelo → el proceso espera a que termine y sale con código 0. Test en Task 5.
- `.env` world-readable en boot → chmod 0600 con warning que nombre solo la ruta. Test en Task 6.

---

### Task 1: Config — `HTTP_TIMEOUT_MS` y `CORS_ORIGINS`

**Files:**
- Modify: `src/config.rs` (struct `Config`, `Default`, `from_env`, `save_to_env`, `CONFIG_ENV_VARS`, tests)
- Modify: `.env.example`

**Interfaces:**
- Consumes: `u64_env(name, default)` ya existente en `src/config.rs:41`.
- Produces: `Config.http_timeout_ms: u64`, `Config.cors_origins: Vec<String>`; constantes `DEFAULT_HTTP_TIMEOUT_MS: u64 = 30000`, `DEFAULT_CORS_ORIGINS: &str`.

- [ ] **Step 1: Write the failing tests** in `src/config.rs` `mod tests`:

```rust
#[test]
fn http_timeout_zero_is_a_config_error() {
    let previous = std::env::var("HTTP_TIMEOUT_MS").ok();
    // SAFETY: env tests are serialized by the existing CONFIG_ENV_VARS guard.
    std::env::set_var("HTTP_TIMEOUT_MS", "0");
    let err = Config::from_env().expect_err("zero timeout must be rejected");
    assert!(err.to_string().contains("HTTP_TIMEOUT_MS"), "{err}");
    restore_env("HTTP_TIMEOUT_MS", previous);
}

#[test]
fn cors_origins_wildcard_is_a_config_error() {
    let previous = std::env::var("CORS_ORIGINS").ok();
    std::env::set_var("CORS_ORIGINS", "*");
    let err = Config::from_env().expect_err("wildcard origin must be rejected");
    assert!(err.to_string().contains("CORS_ORIGINS"), "{err}");
    restore_env("CORS_ORIGINS", previous);
}

#[test]
fn cors_origins_csv_is_parsed_and_trimmed() {
    let previous = std::env::var("CORS_ORIGINS").ok();
    std::env::set_var("CORS_ORIGINS", " http://localhost:8080 , https://a.example ");
    let config = Config::from_env().expect("valid CSV");
    assert_eq!(config.cors_origins, vec!["http://localhost:8080", "https://a.example"]);
    restore_env("CORS_ORIGINS", previous);
}
```

Use el helper de restauración de env ya presente en el módulo de tests (buscar `restore_env` o el patrón `previous`/`set_var` usado por los tests de admin). Si no existe, añadir un helper local `fn restore_env(name: &str, previous: Option<String>)`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --target-dir target-tests config::tests -- --nocapture`
Expected: FAIL — `Config` no tiene los campos.

- [ ] **Step 3: Implement the config fields**

En `src/config.rs`:
- Añadir `pub http_timeout_ms: u64` y `pub cors_origins: Vec<String>` al struct y a `Default` (`30000` y `vec!["http://localhost".into(), "http://127.0.0.1".into()]`).
- En `from_env`: `let http_timeout_ms = u64_env("HTTP_TIMEOUT_MS", DEFAULT_HTTP_TIMEOUT_MS)?;` seguido de `if http_timeout_ms == 0 { return Err(AppError::Config("HTTP_TIMEOUT_MS must be greater than 0".into())); }`.
- Para CORS: leer `CORS_ORIGINS`, separar por `,`, trim, filtrar vacíos; si el resultado contiene `"*"`, error; si está vacío, usar los defaults. Validar cada entrada con `HeaderValue::from_str` (esto ya rechaza caracteres inválidos antes de que lo haga la capa CORS).
- En `save_to_env`: persistir ambas variables con el mismo patrón que las demás (buscar el bucle de `for (key, value) in [...]`).
- Añadir `"HTTP_TIMEOUT_MS"` y `"CORS_ORIGINS"` a `CONFIG_ENV_VARS`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --target-dir target-tests config::tests`
Expected: PASS (los 3 tests nuevos + los existentes).

- [ ] **Step 5: Update `.env.example`**

Añadir, bajo la sección de red:

```dotenv
# Outbound HTTP timeout (ms). 0 is rejected. Default: 30000.
HTTP_TIMEOUT_MS=30000
# Comma-separated allowed CORS origins. Loopback by default; "*" is rejected.
CORS_ORIGINS=http://localhost,http://127.0.0.1
```

- [ ] **Step 6: Commit**

```bash
git add src/config.rs .env.example
git commit -m "feat(config): add HTTP timeout and CORS allowlist settings"
```

---

### Task 2: `src/net.rs` — fábricas compartidas

**Files:**
- Create: `src/net.rs`
- Modify: `src/lib.rs` (declarar `pub mod net;`)
- Test: `src/net.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `AppError` de `src/error.rs`.
- Produces: `pub fn http_client(timeout: Duration) -> Result<reqwest::Client, AppError>`; `pub fn cors_layer(origins: &[String]) -> Result<CorsLayer, AppError>`.

- [ ] **Step 1: Write the failing tests** in `src/net.rs`:

```rust
#[tokio::test]
async fn http_client_times_out_a_sleeping_server() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        use tokio::io::AsyncWriteExt;
        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
    });
    let client = http_client(Duration::from_millis(200)).unwrap();
    let err = client.get(format!("http://{addr}/")).send().await.unwrap_err();
    assert!(err.is_timeout(), "expected timeout, got {err}");
}

#[test]
fn cors_layer_allows_only_listed_origins() {
    let layer = cors_layer(&["http://localhost:8080".into()]).unwrap();
    // La capa se prueba de verdad en Task 4; aquí solo se construye sin panic.
    let _ = layer;
}

#[test]
fn cors_layer_rejects_wildcard_and_invalid_origin() {
    assert!(cors_layer(&["*".into()]).is_err());
    assert!(cors_layer(&["not a url".into()]).is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --target-dir target-tests net::tests`
Expected: FAIL — módulo no existe.

- [ ] **Step 3: Implement `src/net.rs`**

```rust
pub fn http_client(timeout: Duration) -> Result<reqwest::Client, AppError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5).min(timeout))
        .timeout(timeout)
        .build()
        .map_err(|err| AppError::Runtime(format!("failed to build HTTP client: {err}")))
}

pub fn cors_layer(origins: &[String]) -> Result<CorsLayer, AppError> {
    let mut values = Vec::with_capacity(origins.len());
    for origin in origins {
        if origin == "*" {
            return Err(AppError::Config("CORS_ORIGINS cannot contain '*'".into()));
        }
        let value = HeaderValue::from_str(origin).map_err(|_| {
            AppError::Config(format!("CORS_ORIGINS contains an invalid origin: '{origin}'"))
        })?;
        values.push(value);
    }
    Ok(CorsLayer::new()
        .allow_origin(values)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static("x-project"),
            HeaderName::from_static("x-agent"),
            HeaderName::from_static("x-debug"),
            HeaderName::from_static("x-conversation-id"),
            HeaderName::from_static("x-admin-token"),
        ]))
}
```

Nota: `AllowOrigin` acepta `Vec<HeaderValue>` vía `Into<AllowOrigin>` en tower-http 0.5; si el tipo no convierte directamente, usar `AllowOrigin::list(values)`.

- [ ] **Step 4: Wire the module**

En `src/lib.rs`, añadir `pub mod net;` junto a los otros `pub mod`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --target-dir target-tests net::tests`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/net.rs src/lib.rs
git commit -m "feat(net): add shared HTTP client and CORS factories"
```

---

### Task 3: Aplicar timeouts a proveedores y health

**Files:**
- Modify: `src/providers/ollama.rs:20`
- Modify: `src/providers/openai_compatible.rs:56`
- Modify: `src/providers/opencode.rs:34`
- Modify: `src/adapters/rig_engine/mod.rs` (donde se construye el cliente de OpenRouter/Rig si aplica)
- Test: `tests/provider_timeout_tests.rs` (nuevo)

**Interfaces:**
- Consumes: `net::http_client(Duration)` (Task 2), `Config.http_timeout_ms` (Task 1).
- Produces: ninguna interfaz pública nueva; los tres providers dejan de usar `Client::new()`.

- [ ] **Step 1: Write the failing test** in `tests/provider_timeout_tests.rs`:

```rust
use std::time::{Duration, Instant};

#[tokio::test]
async fn ollama_health_fails_within_the_configured_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
    });
    let provider = llama_r::providers::ollama::OllamaProvider::new(
        format!("http://{addr}"),
        "unused".into(),
        Duration::from_millis(300),
    );
    let started = Instant::now();
    let err = provider.health_check().await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2), "health_check hung: {err}");
}
```

Si el constructor de `OllamaProvider::new` no acepta hoy un `Duration`, añadir un `pub fn with_timeout(base_url, model, timeout)` y usar ese en el test; mantener `new` como wrapper que llama a `with_timeout` con el default de config para no romper llamadores. Aplicar el mismo patrón a los otros dos providers.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --target-dir target-tests --test provider_timeout_tests`
Expected: FAIL — el constructor con timeout no existe / la llamada cuelga.

- [ ] **Step 3: Implement**

En cada provider: sustituir `Client::new()` por `net::http_client(timeout).expect("validated timeout")` (o propagar el error si el constructor ya devuelve `Result`). El timeout se pasa desde `Config.http_timeout_ms` en el punto de construcción (`src/runtime.rs`, buscando `OpenCodeProvider::new` / `OllamaProvider::new` / `OpenAiCompatibleProvider::new`) y desde el `timeout_ms` del perfil cuando exista. `health_check` queda cubierto automáticamente porque usa el mismo `client`.

- [ ] **Step 4: Run the test and the full suite**

Run: `cargo test --target-dir target-tests --test provider_timeout_tests && cargo test --target-dir target-tests`
Expected: PASS; ningún test existente cambia de comportamiento.

- [ ] **Step 5: Commit**

```bash
git add src/providers src/adapters/rig_engine tests/provider_timeout_tests.rs src/runtime.rs
git commit -m "fix(net): bound outbound provider and health requests with a timeout"
```

---

### Task 4: Router — CORS allowlist, body limit y historial admin

**Files:**
- Modify: `src/runtime.rs:336-421` (`build_router`)
- Modify: `src/api/chat_core.rs` (validación temprana de `messages`)
- Test: `tests/hardening_router_tests.rs` (nuevo)

**Interfaces:**
- Consumes: `net::cors_layer(&config.cors_origins)` (Task 2); `require_admin` ya existente en `src/api/admin_auth.rs`.
- Produces: las mismas rutas; `/api/conversations*` ahora dentro del grupo admin.

- [ ] **Step 1: Write the failing tests** in `tests/hardening_router_tests.rs`:

Use el patrón de `tests/admin_auth_tests.rs` para construir un `AppState` de test con `ADMIN_TOKEN=e2e` y un `Router` vía `build_router`.

```rust
#[tokio::test]
async fn conversations_require_admin_token() {
    let app = test_router_with_admin_token("e2e");
    let no_token = app.clone().oneshot(
        Request::builder().uri("/api/conversations")
            .body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);
    let with_token = app.oneshot(
        Request::builder().uri("/api/conversations")
            .header("x-admin-token", "e2e")
            .body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(with_token.status(), StatusCode::OK);
}

#[tokio::test]
async fn oversized_chat_body_is_rejected() {
    let app = test_router_with_admin_token("e2e");
    let big = "x".repeat(3 * 1024 * 1024);
    let res = app.oneshot(
        Request::builder().method("POST").uri("/api/chat")
            .header("content-type", "application/json")
            .body(Body::from(big)).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn cors_rejects_unlisted_origin() {
    let app = test_router_with_admin_token("e2e");
    let res = app.oneshot(
        Request::builder().method("POST").uri("/api/chat")
            .header("origin", "http://evil.example")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#))
            .unwrap()).await.unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --target-dir target-tests --test hardening_router_tests`
Expected: FAIL — conversaciones sin token dan 200; body grande pasa; CORS devuelve `*`.

- [ ] **Step 3: Implement `build_router`**

- Sustituir el bloque `CorsLayer::new().allow_origin(Any)...` por `net::cors_layer(&state.config.cors_origins).expect("validated at config load")`.
- Quitar las cuatro rutas `/api/conversations...` del router `public` y añadirlas al router `admin` (el que ya lleva `.layer(from_fn_with_state(state.clone(), require_admin))`).
- Añadir `.layer(DefaultBodyLimit::max(2 * 1024 * 1024))` al router final (después de `merge`, antes de `with_state` o después — verificar que no interfiera con el límite de 1 MB de webhooks, que se aplica en el handler y por tanto es más estricto).
- En `src/api/chat_core.rs`, mover la comprobación de `messages.is_empty()` al inicio de `execute_chat`, antes de resolver agente/proyecto; devolver el mismo `AppError::Validation` de hoy.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --target-dir target-tests --test hardening_router_tests && cargo test --target-dir target-tests`
Expected: PASS; los tests de conversaciones existentes pueden necesitar el header `x-admin-token` — actualizarlos en este mismo task.

- [ ] **Step 5: Commit**

```bash
git add src/runtime.rs src/api/chat_core.rs tests/hardening_router_tests.rs tests/admin_auth_tests.rs
git commit -m "fix(router): allowlist CORS, cap body size and gate conversation history"
```

---

### Task 5: Graceful shutdown con `SIGTERM`

**Files:**
- Modify: `src/main.rs:113-131`
- Modify: `src/runtime.rs:817-835`, `src/runtime.rs:1589`
- Test: `tests/graceful_shutdown_tests.rs` (nuevo)

**Interfaces:**
- Consumes: `tokio::signal`, `axum::serve(...).with_graceful_shutdown(future)`, `tonic::Server::serve_with_shutdown`.
- Produces: `pub async fn shutdown_signal()` en `src/runtime.rs`, usada por `main`.

- [ ] **Step 1: Write the failing test** in `tests/graceful_shutdown_tests.rs`:

```rust
#[tokio::test]
async fn shutdown_signal_completes_on_sigterm() {
    // En Linux: spawn `kill -TERM <pid>` contra el binario de test, o
    // señalizar el proceso actual con `libc::raise(libc::SIGTERM)` y esperar
    // `llama_r::runtime::shutdown_signal()` con timeout de 2 s.
    let signal = tokio::time::timeout(
        Duration::from_secs(2),
        llama_r::runtime::shutdown_signal(),
    );
    assert!(signal.await.is_ok(), "SIGTERM did not resolve shutdown_signal");
}
```

Si señalizar el proceso de test resulta inestable, usar en su lugar un E2E con el binario real (arrancar `--server-only`, enviar `SIGTERM`, assert de exit 0) y dejar este test unitario como `#[cfg(unix)]`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --target-dir target-tests --test graceful_shutdown_tests`
Expected: FAIL — `shutdown_signal` no existe.

- [ ] **Step 3: Implement**

- En `src/runtime.rs`, añadir:

```rust
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "Failed to listen for Ctrl-C");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => { stream.recv().await; }
            Err(err) => tracing::error!(error = %err, "Failed to listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    tracing::info!("shutdown signal received");
}
```

- En `start_http_server`: `axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await`.
- En `start_grpc_server`: usar `serve_with_shutdown(addr, service, shutdown_signal())` (o el equivalente del tonic 0.14 presente en el repo).
- En `main.rs`: en la rama `server_only`, esperar `shutdown_signal()`; en la rama TUI, al terminar `tui.run()` llamar también `shutdown_signal().await` para disparar el mismo camino. Después, en vez de `http_server.abort(); grpc_server.abort();`, hacer `let _ = http_server.await; let _ = grpc_server.await;`.
- `api_running` se limpia como hoy cuando `axum::serve` termina.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --target-dir target-tests --test graceful_shutdown_tests && cargo test --target-dir target-tests`
Expected: PASS. Verificar además el E2E manual: arrancar, `kill -TERM`, el proceso termina en < 5 s con código 0.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/runtime.rs tests/graceful_shutdown_tests.rs
git commit -m "fix(shutdown): drain in-flight requests on Ctrl-C or SIGTERM"
```

---

### Task 6: `.env` 0600 en boot

**Files:**
- Modify: `src/config.rs` (`from_env`, helper `restrict_env_permissions`)
- Modify: `tests/env_permissions_tests.rs` (nuevo)

**Interfaces:**
- Consumes: helper de permisos unix ya usado en `save_to_env` (`std::os::unix::fs::PermissionsExt`).
- Produces: `fn restrict_env_permissions(path: &Path)` (privado, `#[cfg(unix)]`).

- [ ] **Step 1: Write the failing test** in `tests/env_permissions_tests.rs`:

```rust
#[cfg(unix)]
#[test]
fn boot_restricts_world_readable_env_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("llama-r-env-perms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let env_path = dir.join(".env");
    std::fs::write(&env_path, "DEFAULT_MODEL=x").unwrap();
    std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    // Config::from_env lee del .env del cwd; el helper debe poder aplicarse
    // a una ruta explícita para ser testeable sin mutar el cwd del proceso.
    llama_r::config::restrict_env_permissions(&env_path);
    let mode = std::fs::metadata(&env_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    std::fs::remove_dir_all(&dir).unwrap();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --target-dir target-tests --test env_permissions_tests`
Expected: FAIL — función no existe / no pública.

- [ ] **Step 3: Implement**

En `src/config.rs`: extraer el bloque `#[cfg(unix)]` de `save_to_env` en `pub fn restrict_env_permissions(path: &Path)`; llamarlo desde `save_to_env` y desde `from_env` justo tras `dotenv().ok()`, usando `crate::core::paths::get_base_dir().join(".env")` (o la ruta que `dotenvy` resolvió — si no es accesible, comprobar también `.env` del cwd). El warning MUST nombrar solo la ruta, nunca el contenido.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --target-dir target-tests --test env_permissions_tests && cargo test --target-dir target-tests`
Expected: PASS. Verificar además `stat -c %a .env` del repo ⇒ `600`.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs tests/env_permissions_tests.rs .env
git commit -m "fix(config): restrict .env permissions to 0600 on boot"
```

Nota: no commitear el contenido del `.env`; si el repo no lo trackea, `git add .env` no hará nada y está bien — el chmod se aplica al filesystem, no al índice.

---

### Task 7: Documentación y verificación E2E completa

**Files:**
- Modify: `AGENTS.md`
- Modify: `README.md`
- Test: script temporal fuera del repo (`/tmp/opencode/llama-r-hardening-e2e.sh`)

**Interfaces:**
- Consumes: todos los tasks anteriores.
- Produces: documentación operativa de deployment LAN.

- [ ] **Step 1: Update `AGENTS.md`**

Añadir a la sección de Environment las dos variables nuevas con default y validación; a la sección de HTTP API, en "Conversation History API", avisar de que requiere `X-Admin-Token` (breaking change) y que el grupo admin ya cubre esas rutas. Añadir una nota de deployment headless: `SIGTERM` ahora drena peticiones, y `HTTP_TIMEOUT_MS`/`CORS_ORIGINS` son obligatorias de revisar en LAN.

- [ ] **Step 2: Update `README.md`**

Mismo contenido en forma de guía de uso: cómo añadir un origen web a `CORS_ORIGINS`, cómo llamar al historial con el token, y el nuevo comportamiento de apagado. No tocar ejemplos de chat ni puertos.

- [ ] **Step 3: Run the full E2E script**

El script debe, con puerto libre y `LLAMA_R_DIR` aislado:
1. Arrancar `ADMIN_TOKEN=e2e ... llama-r --server-only`.
2. `GET /api/health` ⇒ 200 en < 5 s.
3. `GET /api/conversations` sin token ⇒ 401; con token ⇒ 200.
4. `POST /api/chat` con body 3 MB ⇒ 413.
5. `curl -H 'Origin: http://evil.example'` ⇒ sin `Access-Control-Allow-Origin`; `-H 'Origin: http://localhost:3000'` ⇒ header presente.
6. `kill -TERM <pid>` ⇒ exit 0 en < 5 s.
7. `stat -c %a .env` ⇒ 600.

- [ ] **Step 4: Run the full regression suite**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --target-dir target-tests`
Expected: todo PASS; 963 tests verdes; ningún servidor de prueba vivo al terminar.

- [ ] **Step 5: Commit**

```bash
git add AGENTS.md README.md
git commit -m "docs: document LAN hardening, admin-gated history and graceful shutdown"
```
