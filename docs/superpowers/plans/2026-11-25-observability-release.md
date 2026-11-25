# Observabilidad y release (Fase 10-B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Exponer los 16 contadores existentes como un endpoint Prometheus público y dejar el repo publicable con CI reproducible y release versionada 0.2.0.

**Architecture:** `src/api/metrics.rs` formatea `ObservabilitySnapshot` a texto Prometheus sin dependencias nuevas; la ruta se añade al router público. CI gana toolchain fijado, cargo-audit versionado, cargo-deny, coverage local y build release; `Cargo.toml`/LICENSE/CHANGELOG y un `release.yml` completan la publicación.

**Tech Stack:** Rust 1.97, axum 0.7, utoipa 5, `cargo-llvm-cov`, `cargo-deny`, GitHub Actions, `softprops/action-gh-release@v2`.

**Spec:** `docs/superpowers/specs/2026-11-25-observability-release-design.md`

## Global Constraints

- `/metrics` público, `text/plain; version=0.0.4; charset=utf-8`, prefijo `llamar_`, 16 métricas exactas de la tabla del spec §3.1.
- `cost_usd_total_micros` se expone en **dólares** (`micros as f64 / 1e6`), nunca fabricado; sin claves/URLs/ids en etiquetas.
- Sin histogramas ni dependencia Prometheus nueva.
- Versión `0.2.0`; license `MIT`; repository `https://github.com/LEMNXCIX/llama-r`; tag `v0.2.0` (aprobado).
- Dependabot `package-ecosystem: "cargo"`; `cargo-audit --version 0.22.0 --locked`; `cargo llvm-cov --fail-under 40`; `cargo build --release`.
- Release matrix Linux/macOS/Windows, **sin Docker/systemd**; sin Codecov/token.
- `cargo test --target-dir target-tests` MUST pasar (985+ tests); fmt/clippy limpios.

## Review Focus

- `render()` con micros `1_234_567` → `llamar_cost_usd_total 1.234567` (no `1234567`). Test en Task 1.
- `/metrics` sin `X-Admin-Token` sigue 200 y no filtra secretos ni ids. Test en Task 1 (unit) + E2E en Task 5.
- `avg_chat_latency_ms` es gauge derivado; `completed_chat_requests` (interno) NO se expone. Test en Task 1.
- Dependabot con `package-ecosystem: ""` no actualiza nada → debe ser `"cargo"`. Verificación en Task 3 (diff review).
- `deny.toml` con `allow` que oculte advisories → prohibido; cualquier excepción justificada. Verificación en Task 3.
- Release workflow con `GITHUB_TOKEN` implícito, sin secretos nuevos. Verificación en Task 4.

---

### Task 1: Endpoint `/metrics` + test de formato

**Files:**
- Create: `src/api/metrics.rs`
- Modify: `src/runtime.rs` (import + `.route("/metrics", get(metrics))` en el router público)
- Modify: `src/api/docs.rs` (añadir `crate::api::metrics::metrics` a `paths(...)` y `version = "0.2.0"`)
- Test: `src/api/metrics.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `ObservabilitySnapshot` de `src/api/observability.rs` (16 campos públicos); `AppState.observability: AppObservability`.
- Produces: `pub fn render(snapshot: &ObservabilitySnapshot) -> String`; `pub async fn metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse`.

- [ ] **Step 1: Write the failing tests** in `src/api/metrics.rs`:

```rust
#[test]
fn render_emits_all_sixteen_metrics_with_prometheus_shape() {
    let snapshot = ObservabilitySnapshot {
        http_requests: 10, chat_requests: 2, fallback_count: 1,
        provider_errors: 0, grpc_errors: 0, mcp_errors: 0,
        chat_latency_ms_total: 500, avg_chat_latency_ms: 250,
        routed_to_agent: 1, routed_to_general: 1, router_errors: 0,
        router_latency_ms_total: 40, auto_approved: 1, auto_denied: 0,
        auto_unavailable: 0, cost_usd_total_micros: 1_234_567,
    };
    let text = render(&snapshot);
    assert!(text.contains("# HELP llamar_chat_requests_total"));
    assert!(text.contains("# TYPE llamar_chat_requests_total counter"));
    assert!(text.contains("llamar_chat_requests_total 2"));
    assert!(text.contains("llamar_avg_chat_latency_ms 250"));
    assert!(text.contains("llamar_cost_usd_total 1.234567"));
    assert_eq!(text.lines().filter(|l| l.starts_with("llamar_")).count(), 16);
    assert!(!text.to_ascii_lowercase().contains("key"));
    assert!(!text.to_ascii_lowercase().contains("token"));
}

#[test]
fn render_does_not_expose_internal_completed_counter() {
    let text = render(&ObservabilitySnapshot::default());
    assert!(!text.contains("completed_chat_requests"));
}
```

Use `ObservabilitySnapshot::default()` si deriva `Default`; si no, construya el struct con ceros.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --target-dir target-tests metrics::tests`
Expected: FAIL — módulo/función no existe.

- [ ] **Step 3: Implement `render` and the handler**

`render` SHALL emitir, para cada campo del snapshot en el orden de la tabla §3.1, tres líneas: `# HELP <name> <texto>`, `# TYPE <name> <counter|gauge>`, `<name> <valor>`. Coste: `micros as f64 / 1e6` formateado con `{}` (dará `1.234567`). El handler incrementa `record_http_request()` y devuelve `(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8"), render(...)`. No reutilice el contador de `health` directamente — use `snapshot()`.

- [ ] **Step 4: Wire the route and OpenAPI**

En `src/runtime.rs`: importar `use crate::api::metrics::metrics;` y añadir `.route("/metrics", get(metrics))` al router `public` junto a `/health`. En `src/api/docs.rs`: añadir `crate::api::metrics::metrics` a `paths(...)` y cambiar `version = "0.1.0"` por `"0.2.0"`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --target-dir target-tests metrics::tests && cargo test --target-dir target-tests`
Expected: PASS; suite verde.

- [ ] **Step 6: Commit**

```bash
git add src/api/metrics.rs src/runtime.rs src/api/docs.rs
git commit -m "feat(metrics): expose Prometheus text endpoint at /metrics"
```

---

### Task 2: Metadata de release — versión, LICENSE, CHANGELOG

**Files:**
- Modify: `Cargo.toml`
- Create: `LICENSE`, `CHANGELOG.md`

**Interfaces:**
- Consumes: ninguno.
- Produces: `llama-r 0.2.0` con metadata; `LICENSE` MIT; `CHANGELOG.md` con sección `[0.2.0]`.

- [ ] **Step 1: Write the failing test**

No hay test de código. Cree `tests/release_metadata_tests.rs` con un test que lea `Cargo.toml` como texto y compruebe:

```rust
#[test]
fn cargo_toml_declares_publication_metadata() {
    let manifest = std::fs::read_to_string("Cargo.toml").unwrap();
    assert!(manifest.contains("version = \"0.2.0\""));
    assert!(manifest.contains("license = \"MIT\""));
    assert!(manifest.contains("repository = \"https://github.com/LEMNXCIX/llama-r\""));
    assert!(manifest.contains("description ="));
    assert!(std::path::Path::new("LICENSE").exists());
    let changelog = std::fs::read_to_string("CHANGELOG.md").unwrap();
    assert!(changelog.contains("## [0.2.0] - 2026-11-25"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --target-dir target-tests --test release_metadata_tests`
Expected: FAIL.

- [ ] **Step 3: Implement**

En `Cargo.toml` `[package]`: `version = "0.2.0"`, `license = "MIT"`, `repository = "https://github.com/LEMNXCIX/llama-r"`, `description = "Personal AI gateway with HTTP, OpenAI-compatible and gRPC APIs"`, `authors = ["LEMNXCIX"]`, `readme = "README.md"`. Crear `LICENSE` con el texto MIT completo (Copyright 2026 LEMNXCIX). Crear `CHANGELOG.md` con `## [0.2.0] - 2026-11-25` y bullets: hardening de red (A), `/metrics`, CI/release, y breaking change de historial bajo admin token.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --target-dir target-tests --test release_metadata_tests && cargo test --target-dir target-tests`
Expected: PASS. `Cargo.lock` puede actualizar su campo `version` para llama-r — commitéelo.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock LICENSE CHANGELOG.md tests/release_metadata_tests.rs
git commit -m "chore(release): bump to 0.2.0 and add publication metadata"
```

---

### Task 3: CI — Dependabot, audit pin, cargo-deny, coverage, toolchain

**Files:**
- Modify: `.github/dependabot.yml`
- Modify: `.github/workflows/ci.yml`
- Create: `deny.toml`, `rust-toolchain.toml`

**Interfaces:**
- Consumes: `Cargo.toml` 0.2.0 (Task 2).
- Produces: CI reproducible con 5 gates.

- [ ] **Step 1: Write the failing checks** as a shell script `tests/ci_config_tests.sh` (o un test Rust que lea los YAML):

```bash
#!/usr/bin/env bash
set -euo pipefail
grep -q 'package-ecosystem: "cargo"' .github/dependabot.yml
! grep -q 'package-ecosystem: ""' .github/dependabot.yml
grep -q 'cargo-audit --version 0.22.0' .github/workflows/ci.yml
grep -q 'llvm-tools-preview' .github/workflows/ci.yml
grep -q 'cargo llvm-cov' .github/workflows/ci.yml
grep -q -- '--fail-under 40' .github/workflows/ci.yml
grep -q 'cargo deny check' .github/workflows/ci.yml
grep -q 'cargo build --release' .github/workflows/ci.yml
test -f deny.toml && test -f rust-toolchain.toml
echo CI_CONFIG_OK
```

- [ ] **Step 2: Run to verify it fails**

Run: `bash tests/ci_config_tests.sh`
Expected: FAIL en el primer grep.

- [ ] **Step 3: Implement CI config**

- `dependabot.yml`: `package-ecosystem: "cargo"`, `open-pull-requests-limit: 5`.
- `rust-toolchain.toml`: `[toolchain] channel = "stable"; components = ["clippy","rustfmt","llvm-tools-preview"]`.
- `deny.toml`: `[advisories] yanked = "warn"`; `[licenses] allow` con MIT, Apache-2.0, BSD-3-Clause, ISC, Unicode-DFS-2016, MPL-2.0, CC0-1.0; `confidence-threshold = 0.8`; `[bans] multiple-versions = "warn"`, `highlight = "all"`; **sin entradas `[advisories] ignore`**.
- `ci.yml`: toolchain components `clippy, rustfmt, llvm-tools-preview`; `cargo install cargo-audit --version 0.22.0 --locked`; tras tests, `cargo install cargo-llvm-cov --locked` y `cargo llvm-cov --all-features --target-dir target-tests --lcov --output-path lcov.info --fail-under 40`; `cargo deny check`; `cargo build --release --target-dir target-release`.

- [ ] **Step 4: Run to verify it passes**

Run: `bash tests/ci_config_tests.sh`
Expected: `CI_CONFIG_OK`.
Además ejecutar localmente: `cargo deny check` y `cargo llvm-cov --all-features --target-dir target-tests --lcov --output-path /tmp/opencode/llama-r-b/lcov.info --fail-under 40`. Si `--fail-under 40` falla por cobertura real menor, **documentar el porcentaje exacto en el report** y ajustar el umbral al alza o a la baja solo con justificación escrita en el report (nunca silenciosamente).

- [ ] **Step 5: Commit**

```bash
git add .github/dependabot.yml .github/workflows/ci.yml deny.toml rust-toolchain.toml tests/ci_config_tests.sh
git commit -m "ci: pin toolchain and audit, add deny, coverage and release build"
```

---

### Task 4: Release workflow con binarios por OS

**Files:**
- Create: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: `deny.toml` + `rust-toolchain.toml` (Task 3), metadata 0.2.0 (Task 2).
- Produces: workflow que, al pushear `v*`, publica artifacts por OS y GitHub Release.

- [ ] **Step 1: Write the failing check**

Añada a `tests/ci_config_tests.sh`:

```bash
test -f .github/workflows/release.yml
grep -q 'tags:' .github/workflows/release.yml
grep -q 'matrix:' .github/workflows/release.yml
grep -q 'macos-latest' .github/workflows/release.yml
grep -q 'windows-latest' .github/workflows/release.yml
grep -q 'action-gh-release' .github/workflows/release.yml
! grep -q 'docker' .github/workflows/release.yml
```

- [ ] **Step 2: Run to verify it fails**

Run: `bash tests/ci_config_tests.sh`
Expected: FAIL en `test -f release.yml`.

- [ ] **Step 3: Implement `release.yml`**

Triggers: `push: tags: ["v*"]` y `workflow_dispatch`. Job `build` con `matrix.os: [ubuntu-latest, macos-latest, windows-latest]`; pasos: checkout, toolchain desde `rust-toolchain.toml`, `cargo build --release --locked`, `actions/upload-artifact@v4` con nombre `llama-r-${{ matrix.os }}` y path del binario (`target/release/llama-r` o `.exe`). Job `release` con `needs: build`, `runs-on: ubuntu-latest`, `permissions: contents: write`, descarga de artifacts y `softprops/action-gh-release@v2` con `body_path: CHANGELOG.md`. Sin secretos nuevos (usa `GITHUB_TOKEN` implícito). Sin Docker/systemd.

- [ ] **Step 4: Run to verify it passes**

Run: `bash tests/ci_config_tests.sh`
Expected: `CI_CONFIG_OK`.
Validación estática adicional: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/release.yml'))"` (si PyYAML disponible; si no, revise el YAML a mano y anótelo en el report).

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/release.yml tests/ci_config_tests.sh
git commit -m "ci(release): add tag-triggered binary release workflow"
```

---

### Task 5: Docs, E2E y cierre con tag

**Files:**
- Modify: `README.md`, `AGENTS.md`
- Test: script temporal `/tmp/opencode/llama-r-b-e2e.sh`

**Interfaces:**
- Consumes: `/metrics` (Task 1), versión 0.2.0 (Task 2), CI (Task 3), release (Task 4).
- Produces: repo documentado y tag `v0.2.0` pusheado.

- [ ] **Step 1: Update docs**

En `README.md`: badge de CI (`https://github.com/LEMNXCIX/llama-r/actions/workflows/ci.yml/badge.svg`), sección "Métricas" con `curl http://127.0.0.1:3000/metrics` y nota de que es público y sin secretos. En `AGENTS.md`: añadir `GET /metrics` al HTTP API Quick Reference y una nota en Developer Commands de que CI corre fmt/audit/deny/coverage/clippy/tests/release-build y que el toolchain está en `rust-toolchain.toml`. No cambiar ejemplos de chat ni puertos.

- [ ] **Step 2: Run the E2E script** (`/tmp/opencode/llama-r-b-e2e.sh`):

1. Arrancar binario en puerto libre con `LLAMA_R_DIR` aislado y Ollama disponible.
2. `GET /metrics` sin headers ⇒ 200, content-type Prometheus, contiene `llamar_chat_requests_total 0`.
3. `POST /api/chat` con `llama3.2:latest` ⇒ 200; `GET /metrics` ⇒ `llamar_chat_requests_total 1`.
4. `GET /metrics` con/sin `X-Admin-Token` ⇒ ambos 200.
5. `curl -s /metrics | grep -iE 'key|token|secret'` ⇒ sin salida.
6. `cargo audit`, `cargo deny check`, `cargo llvm-cov ... --fail-under 40` ⇒ todos exit 0.
7. Matar el servidor.

Capturar la salida en el report; ningún proceso vivo al final.

- [ ] **Step 3: Run full regression**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --target-dir target-tests && bash tests/ci_config_tests.sh`
Expected: todo verde; 985+ tests.

- [ ] **Step 4: Commit docs**

```bash
git add README.md AGENTS.md
git commit -m "docs: document metrics endpoint and CI toolchain"
```

- [ ] **Step 5: Create and push the tag**

```bash
git tag -a v0.2.0 -m "llama-r 0.2.0 — observability and release (Fase 10-B)"
git push origin master
git push origin v0.2.0
```

Criterio de aceptación: E2E verde, CI local verde, tag `v0.2.0` visible en `origin`.
