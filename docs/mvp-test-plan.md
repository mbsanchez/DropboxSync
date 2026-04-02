# MVP Test Plan (2 semanas)

## Alcance MVP
- Login OAuth Dropbox funcional.
- Guardado seguro de token en keychain.
- Seleccion de carpeta local y estado inicial de sync.
- Cola de jobs basica con retries y backoff.
- UI minima de estado y actividad.

## Criterios de aceptacion testables
1. **OAuth exitoso**
   - Dado cliente configurado, cuando usuario completa OAuth, entonces app muestra `Authenticated`.
2. **Token seguro**
   - Token no aparece en logs ni en SQLite.
3. **Config carpeta**
   - Al definir carpeta, `get_sync_status` devuelve `trackedPath`.
4. **Retry/backoff**
   - Ante error simulado, job incrementa `attempt_count` y calcula `next_retry_at`.
5. **Persistencia DB**
   - `sync_jobs` y `sync_conflicts` existen y se crean al iniciar.
6. **Build instalable macOS**
   - CI produce artefacto bundler de Tauri en macOS.

## Pruebas locales punta a punta
1. Exportar variables:
   - `DROPBOX_APP_KEY`
   - `DROPBOX_REDIRECT_URI` (ej. `http://localhost:53682/callback`)
2. Ejecutar app con `npm run dev` en raiz.
3. Pulsar "Start Dropbox Login", autenticar en browser y permitir acceso.
4. Verificar callback automatico hacia `DROPBOX_REDIRECT_URI` y cierre de flujo.
5. Confirmar en app mensaje de autenticacion completada.
6. Definir carpeta local y verificar estado en UI.
7. Forzar error de red para validar retries.


## Semana 2 - pruebas de integracion
1. Configurar carpeta de sync con al menos 3 archivos.
2. Ejecutar `Run Sync Tick` y verificar en cola jobs `upload` para nuevos archivos.
3. Modificar un archivo y correr nuevo tick: debe encolar nuevo `upload`.
4. Borrar un archivo y correr tick: debe encolar `delete`.
5. Crear/renombrar archivo con `retry` en nombre y correr ticks consecutivos: debe verse `retry_wait` con backoff creciente y luego `done`.
6. Modificar archivo durante job pendiente para disparar `conflicted copy` y registro en conflictos.

## Semana 3 - On-demand sync + UX

## Criterios de aceptacion testables
1. Al iniciar la app, no se descarga ningun archivo/contendido remoto automaticamente.
2. Al click en `Load remote root`, se lista el arbol remoto y se muestran placeholders/estados por archivo.
3. Al click en `Sync file` se descarga un archivo individual en la carpeta local configurada.
4. Al click en `Open` para carpetas, se navega sin descargar; al click en `Sync folder` se hidrata recursivamente.
5. Filtros include/exclude afectan que se descargue/hidrate (carpetas excluidas muestran `Excluded`).

## Pruebas locales
- Configurar scopes y reautorizar OAuth.
- Configurar `Sync folder` (ej. /Users/mobsanchez/DropboxSync).
- No ejecutar ningun sync automatico; cargar remote root.
- Sync de 1 archivo y validar descarga.
- Sync de carpeta y validar creacion de estructura local.
