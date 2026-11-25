# Diseño: observabilidad y release (Fase 10-B)

- **Fecha:** 2026-11-25
- **Estado:** Diseño aprobado en brainstorming; pendiente de revisión del spec
- **Tipo:** Arquitectural — añade un endpoint público de métricas y cambia el pipeline de CI/release
- **Perfil de deployment:** LAN / localhost, uso personal (hereda Fase 10-A)
- **Parte:** 2 de 3 del plan de puesta a punto (A = hardening ✅; B = observabilidad/release; C = brechas funcionales)

## 1. Contexto y problema

La auditoría de 2026-11-24 encontró que llama-r tiene 16 contadores atómicos en `AppObservability` (`src/api/observability.rs:5-27`), expuestos solo dentro de `GET /api/health` (`src/api/health.rs:49,111`), sin un endpoint de métricas estándar que pueda consumir Prometheus/Grafana. Además:

- `.github/dependabot.yml:8` tiene `package-ecosystem: ""` → no actualiza nada.
- `cargo-audit` se instala sin fijar versión en CI (`.github/workflows/ci.yml:36-37`); no hay `deny.toml` ni `audit.toml`.
- No hay coverage gate, build `--release`, `rust-toolchain.toml`, LICENSE, CHANGELOG ni tags. `Cargo.toml:3` sigue en `0.1.0` y no tiene `license`/`repository`/`description`/`authors`.
- No hay release workflow, Docker ni systemd (Docker/systemd quedan fuera de alcance por decisión del usuario).

Objetivo: exponer las métricas existentes en formato Prometheus sin añadir dependencias, y dejar el repo publicable y verificable en CI con una release mínima versionada.

## 2. Decisiones

| # | Decisión | Justificación |
|---|---|---|
| D1 | `GET /metrics` **público**, formato Prometheus text 0.0.4, generado a mano desde `ObservabilitySnapshot` con `write!`. | Los 16 contadores son `AtomicU64`; no hay crate de encoding en el lockfile. Añadir `prometheus` sería una dependencia innecesaria. |
| D2 | Prefijo de métricas `llamar_` (no `llama_`). | Evita colisión con métricas de llama.cpp/Ollama en el mismo scrape. |
| D3 | Solo contadores + un gauge derivado (`avg_chat_latency_ms`); **sin histogramas**. | Los buckets exigirían nuevos contadores por request; coste desproporcionado. Queda documentado como deferido. |
| D4 | `cost_usd_total_micros` se expone como `llamar_cost_usd_total` en **dólares** (`micros as f64 / 1e6`), nunca fabricado. | Consistente con la política de coste real del proyecto (`AGENTS.md`). |
| D5 | Dependabot `package-ecosystem: "cargo"`; `cargo-audit` fijado con `--version`; añadir `deny.toml` (advisories + licenses + banned duplicates) y `rust-toolchain.toml`. | Arregla los dos defectos de CI y hace reproducible el toolchain. |
| D6 | Coverage con `cargo-llvm-cov` sobre la misma suite `target-tests`, con `--fail-under` local. **Sin Codecov/token**. | Gate simple, sin secretos nuevos ni servicio externo. |
| D7 | Release workflow con binarios por OS (Linux/macOS/Windows), **sin Docker/systemd**. Versión `0.2.0`, tag `v0.2.0`, LICENSE MIT, CHANGELOG. | Usuario eligió MIT + sin Docker. El tag lo crea el agente como parte del cierre. |
| D8 | `/metrics` no usa `X-Debug` ni admin token (como `/health`). | Es telemetría agregada y sin secretos; no expone conversaciones ni configuración. |

RFC 2119: **MUST**, **SHALL**, **SHOULD**, **MAY**, **MUST NOT**.

## 3. Endpoint `/metrics`

### 3.1 Formato

Respuesta `200 text/plain; version=0.0.4; charset=utf-8` con, para cada métrica:

```
# HELP llamar_chat_requests_total Total chat requests handled.
# TYPE llamar_chat_requests_total counter
llamar_chat_requests_total 42
```

Métricas a emitir (16 del snapshot, renombradas):

| Snapshot field | Métrica Prometheus | Tipo |
|---|---|---|
| `http_requests` | `llamar_http_requests_total` | counter |
| `chat_requests` | `llamar_chat_requests_total` | counter |
| `fallback_count` | `llamar_fallback_count_total` | counter |
| `provider_errors` | `llamar_provider_errors_total` | counter |
| `grpc_errors` | `llamar_grpc_errors_total` | counter |
| `mcp_errors` | `llamar_mcp_errors_total` | counter |
| `chat_latency_ms_total` | `llamar_chat_latency_ms_total` | counter |
| `avg_chat_latency_ms` | `llamar_avg_chat_latency_ms` | gauge |
| `routed_to_agent` | `llamar_routed_to_agent_total` | counter |
| `routed_to_general` | `llamar_routed_to_general_total` | counter |
| `router_errors` | `llamar_router_errors_total` | counter |
| `router_latency_ms_total` | `llamar_router_latency_ms_total` | counter |
| `auto_approved` | `llamar_auto_approved_total` | counter |
| `auto_denied` | `llamar_auto_denied_total` | counter |
| `auto_unavailable` | `llamar_auto_unavailable_total` | counter |
| `cost_usd_total_micros` | `llamar_cost_usd_total` | gauge (dólares = micros/1e6) |

`completed_chat_requests` es interno y **no** se expone (ya está implícito en `avg`). Ningún campo lleva credenciales, URLs con key ni IDs de conversación.

### 3.2 Implementación

- `src/api/metrics.rs` (nuevo): `pub fn render(snapshot: &ObservabilitySnapshot) -> String` y `pub async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse`. El handler llama a `state.observability.record_http_request()` y luego `render(&state.observability.snapshot())`.
- `src/runtime.rs`: añadir `.route("/metrics", get(metrics))` al router público (junto a `/health`).
- `src/api/mod.rs` o `handlers.rs`: re-export del handler según el patrón existente.
- `utoipa`: añadir el path a `OpenApiDoc` si el repo documenta `/health` allí (buscar `serve_openapi_json`); si no, omitir.

## 4. CI / Dependabot / cargo-deny

### 4.1 `.github/workflows/ci.yml`

- Añadir `llvm-tools-preview` a `dtolnay/rust-toolchain@stable` (componentes: clippy, rustfmt, llvm-tools-preview).
- Fijar `cargo-audit`: `cargo install cargo-audit --version 0.22.0 --locked` (o la versión estable disponible; documentar la elegida).
- Añadir paso de coverage **después** de tests: `cargo install cargo-llvm-cov --locked`, luego `cargo llvm-cov --all-features --target-dir target-tests --lcov --output-path lcov.info --fail-under 40` (umbral inicial conservador; documentar que se subirá).
- Añadir `cargo deny check` con `deny.toml` (advisories defaults; `allow` vacío; `multiple-versions = "warn"`; licenses: permitir MIT/Apache-2.0/BSD-3-Clause/ISC/Unicode-DFS-2016/MPL-2.0 y `CC0-1.0`, denegar `GPL-*` salvo excepción documentada).
- Añadir `cargo build --release --target-dir target-release` a `ci.yml` (o al workflow de release).
- Añadir `rust-toolchain.toml` con `channel = "stable"` y los mismos componentes.

### 4.2 `.github/dependabot.yml`

`package-ecosystem: "cargo"` (antes `""`). Mantener `directory: "/"`, `schedule: weekly`. Añadir `open-pull-requests-limit: 5`.

### 4.3 `.github/workflows/release.yml` (nuevo)

- Trigger: `push: tags: ["v*"]` y `workflow_dispatch`.
- Job `build` con matrix `os: [ubuntu-latest, macos-latest, windows-latest]`.
- Pasos: checkout, toolchain desde `rust-toolchain.toml`, `cargo build --release --locked`, subir artifact `llama-r-<os>` con el binario.
- Job `release` (solo linux, `needs: build`): descargar artifacts y crear GitHub Release con notas desde `CHANGELOG.md`. Usar `softprops/action-gh-release@v2` o `gh release create`; sin secretos nuevos más allá de `GITHUB_TOKEN` implícito.

## 5. Release hygiene

- `Cargo.toml` `[package]`: `version = "0.2.0"`, `license = "MIT"`, `repository = "https://github.com/LEMNXCIX/llama-r"`, `description = "Personal AI gateway with HTTP, OpenAI-compatible and gRPC APIs"`, `authors = ["LEMNXCIX"]`, `readme = "README.md"`.
- `LICENSE`: texto MIT completo, copyright 2026 LEMNXCIX.
- `CHANGELOG.md`: sección `## [0.2.0] - 2026-11-25` con: hardening de red (A), `/metrics`, CI/release, y breaking change de historial bajo admin token.
- Tag `v0.2.0` sobre el commit de cierre, push del tag.
- `README.md`: añadir badge de CI y sección "Métricas" con un ejemplo de scrape; menciona `GET /metrics`.
- `AGENTS.md`: documentar `/metrics` en la API Quick Reference y las nuevas variables/toolchain en developer commands.

## 6. Seguridad y secretos

- `/metrics` emite solo números agregados; **MUST NOT** incluir etiquetas con rutas, ids, modelos, keys ni conversaciones.
- El coverage se sube como artifact de CI, nunca a un servicio externo; no hay token nuevo.
- El release usa `GITHUB_TOKEN` implícito; ningún otro secreto.
- `deny.toml` no incluye excepciones que oculten advisories; cualquier `allow` debe llevar justificación escrita en el propio archivo.

## 7. Testing y verificación

### 7.1 Unit

- `src/api/metrics.rs::tests`: construir un `ObservabilitySnapshot` conocido y asserts de que `render()` contiene exactamente las 16 líneas `# HELP`/`# TYPE` + valores, que `llamar_cost_usd_total` muestra decimales (p. ej. micros `1_234_567` → `1.234567`), y que no aparece la palabra `key`/`token`/`secret`.

### 7.2 E2E

1. Arrancar el binario con puerto libre y `LLAMA_R_DIR` aislado.
2. `GET /metrics` sin headers ⇒ 200, `content-type: text/plain; version=0.0.4`, cuerpo contiene `llamar_chat_requests_total 0`.
3. Hacer un chat real (Ollama `llama3.2:latest`) ⇒ `GET /metrics` muestra `llamar_chat_requests_total 1`.
4. `GET /metrics` con `X-Admin-Token` ausente sigue 200 (público).
5. `cargo audit` y `cargo deny check` pasan.
6. `cargo llvm-cov ... --fail-under 40` pasa sobre la suite completa.

### 7.3 Regresión

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --target-dir target-tests` (985+ verdes), E2E de Fase 10-A sigue pasando (no se tocó hardening).

## 8. Riesgos

| Riesgo | Mitigación |
|---|---|
| `--fail-under 40` falle en CI por baja cobertura real | Umbral conservador elegido; si falla, se documenta el porcentaje real y se ajusta al alza gradualmente, nunca a la baja silenciosa. |
| `cargo deny` rompa CI por licencias transitivas | Revisar `cargo deny check licenses` antes de merge; añadir solo excepciones justificadas en `deny.toml`. |
| Release matrix falle en macOS/Windows por dependencias nativas (rusqlite/aws-lc) | El job usa el mismo toolchain y features que CI; si una plataforma falla, se documenta y se excluye con justificación, no se oculta. |
| Tag `v0.2.0` sea irreversible | El usuario aprobó crearlo; se crea al final, tras review verde. |

## 9. Fuera de alcance

Docker, systemd, Codecov/token, histogramas de latencia, y todo C (compactación real, step-replay, SSE completo, embeddings OpenCode/Anthropic). TLS/rate limiting/multi-usuario siguen fuera (perfil LAN).

## 10. Plan de implementación (siguiente paso)

Tras la revisión de este spec, invocar `writing-plans` para desglosar:

1. `src/api/metrics.rs` + ruta + test de formato.
2. `Cargo.toml` metadata + versión 0.2.0 + LICENSE + CHANGELOG.
3. CI: Dependabot, cargo-audit pin, cargo-deny, coverage, release build, rust-toolchain.
4. Release workflow + tag.
5. Docs (AGENTS/README) + E2E + regresión completa.

Criterio de aceptación: E2E del §7.2 pasa, CI local (audit/deny/coverage/fmt/clippy/tests) verde, y release workflow con artifacts verificable en el runner.
