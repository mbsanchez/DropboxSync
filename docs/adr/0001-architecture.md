# ADR-0001: Dropbox Sync Desktop Architecture

## Status
Accepted

## Context
Queremos una app de sincronizacion estilo Dropbox/ODrive autogestionada, con foco en robustez, seguridad y costo minimo.

## Decision
Se adopta arquitectura modular en monorepo:

- `apps/desktop`: Shell Tauri + React para UI desktop.
- `apps/desktop/src-tauri`: Host Rust con comandos Tauri y servicios locales.
- `packages/shared`: Tipos compartidos UI/servicios.
- `packages/core-sync`: espacio reservado para extraer motor de sync Rust desacoplado.
- `docs/adr`: decisiones de arquitectura versionadas.

### Componentes principales
1. **Desktop UI (React/TS)**
   - Onboarding: login OAuth Dropbox, seleccion de carpeta local.
   - Estado vivo: cola, progreso, errores y timestamp.
2. **Tauri Host (Rust)**
   - Exponer comandos de backend seguros a la UI.
   - Gestionar ciclo de vida del engine local.
3. **Auth Service (Rust)**
   - OAuth2 Authorization Code + PKCE.
   - Apertura de browser del sistema.
   - Intercambio de `code` por token contra Dropbox API.
4. **Secure Token Store (Rust)**
   - Persistencia en keychain del SO usando `keyring`.
   - Nunca escribir tokens en logs ni SQLite.
5. **Metadata Store (SQLite)**
   - Tablas para estado de sync, cola de jobs, archivo indice y conflictos.
6. **Sync Engine (Rust, incremental)**
   - Watcher local + escaneo inicial.
   - Polling/cursor remoto Dropbox.
   - Cola de jobs con retries y backoff exponencial.
   - Resolucion de conflictos con copia conflicted.
7. **CI/CD (GitHub Actions)**
   - Build y tests en PR.
   - Build bundlers Tauri (DMG/APP) en macOS para alpha.

## Data Flow (MVP)
1. Usuario hace login en UI.
2. UI solicita URL OAuth al backend (`start_oauth_flow`).
3. Browser autentica y Dropbox devuelve `code`.
4. UI entrega `code` a backend (`complete_oauth_flow`).
5. Backend intercambia token y guarda en keychain.
6. Usuario elige carpeta local (`set_sync_folder`).
7. Backend inicializa DB local y scheduler de sync.
8. UI consulta estado (`get_sync_status`) y muestra actividad.

## Consequences
- Seguridad mejorada por uso de almacenamiento seguro OS.
- Menor acoplamiento por modulos backend separados.
- MVP rapido con base extensible a selective sync y conflictos avanzados.
