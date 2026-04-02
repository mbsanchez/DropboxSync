# Agent Traceability - Iteracion 1

## PM-Agent
- Definio MVP de 2 semanas y criterios testables en `docs/mvp-test-plan.md`.
- Priorizo robustez y seguridad sobre features cosmeticas.

## Architect-Agent
- Definio arquitectura modular y flujo de datos en `docs/adr/0001-architecture.md`.
- Estructuro monorepo `apps/`, `packages/`, `docs/`, `scripts/`.

## CoreSync-Agent
- Creo esqueleto de engine en `apps/desktop/src-tauri/src/sync/engine.rs`.
- Definio estado de sync y base para queue/retries.

## Desktop-Agent
- Implemento UI minima de onboarding/estado en `apps/desktop/src/App.tsx`.
- Integro comandos Tauri para login, carpeta y estado.

## Security-Agent
- Implemento OAuth2 PKCE base en `auth/oauth.rs`.
- Guardado de token en keychain via `storage/secure_store.rs`.

## QA-Agent
- Definio checklist y criterios en `docs/mvp-test-plan.md`.

## Release-Agent
- Configuro metadata de bundle Tauri para instalable macOS alpha.

## DevOps-Agent
- Creo pipeline CI en `.github/workflows/ci.yml` para build/test y bundle macOS.


## Semana 2 avance
- CoreSync-Agent: implemento escaneo local con hashing SHA-256, deteccion de altas/cambios/bajas, y encolado de jobs.
- CoreSync-Agent: implemento procesador de cola con retries + exponential backoff y estado `retry_wait`/`failed`.
- Security-Agent: mantuvo flujo OAuth seguro sin exponer tokens en DB/logs.
- Desktop-Agent: agrego dashboard de cola, conflictos y auto-tick de sincronizacion.
- QA-Agent: agrego tests Rust (DB + backoff) y checklist de integracion semana 2.

## Semana 3 avance
- CoreSync-Agent: on-demand remote listing + download/hydrate en comandos separados, sin descarga automática inicial.
- Desktop-Agent: UI de Remote Browser con placeholders (Synced/Not downloaded/Excluded) y loader basado en syncRunning.
- Security-Agent: confirmo tokens solo en keychain; UI no muestra token.
- QA-Agent: validacion via tests Rust + typecheck frontend.
