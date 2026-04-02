# ADR-0002: On-demand Remote Sync (Week 3)

## Status
Accepted

## Context
Se busca un UX tipo ODrive/Dropbox Smart Sync: no descargar nada al inicio, pero sí permitir navegación/hidratación bajo demanda.

El mecanismo elegido debe:
- listar el root remoto para detectar nuevos elementos,
- persistir “estado remoto” localmente sin descargar contenido,
- descargar únicamente cuando el usuario lo solicite (archivo/carpeta).

## Decision
- Persistir placeholders locales con extensión `*.cloudsc` (contienen metadata binaria/serializada).
- **Inicio (best-effort)**: al arrancar la app se lista el root remoto (no recursivo) y se crean placeholders `*.cloudsc` en el `sync folder` (sin descargar).
- **Navegación/Hidratación bajo demanda**:
  - archivo placeholder: `trigger_hydrate_cloudsc_placeholder` descarga el archivo remoto y elimina el placeholder.
  - carpeta placeholder: `trigger_hydrate_cloudsc_placeholder` crea el directorio local, elimina el placeholder y genera placeholders `*.cloudsc` para los hijos inmediatos (no descarga recursiva).
- La fase de “watch + cola local” ignora archivos `*.cloudsc` para evitar jobs fantasma.
- La cola ejecuta jobs reales de `upload` (y `delete`) contra Dropbox para cambios locales sobre archivos ya hidratados.

## Consequences
- El consumo inicial de ancho de banda cae drásticamente.
- Se requiere `files.metadata.read` y `files.content.read`/`files.content.write` en Dropbox OAuth (para listar y descargar y para sincronizar cambios locales).
- El modelo todavía no detecta cambios remotos “en existentes” sin listar metadata (se indexa root para nuevos placeholders; la comparación remota se agrega en iteraciones posteriores).
