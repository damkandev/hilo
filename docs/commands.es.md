# Comandos y configuración

<p align="center">
  <a href="commands.md">English</a> · <b>Español</b>
</p>

Solo necesitas `hilo`. Lo demás está para cuando quieras mirar o cambiar algo. La interfaz de la
línea de comandos está en inglés.

## Instalación

| Comando | |
| --- | --- |
| `hilo` | el asistente; en una máquina ya configurada, un menú |
| `hilo setup HOST [--folder DIR]` | el asistente sin preguntas: configura `HOST` por SSH y empareja esta máquina con él |
| `hilo init [--listen IP:PUERTO] [--public] [--bind IP:PUERTO]` | deja esta máquina como ancla e imprime un código de unión ([Despliegue](deployment.es.md)) |
| `hilo join CÓDIGO@IP:PUERTO` | se une a un ancla |
| `hilo add DIR` | vincula otra carpeta (o un solo repo) |
| `hilo up` | en el script de setup de un sandbox: une, vincula, arranca y espera a quedar al día ([Agentes en la nube](cloud-agents.es.md)) |
| `hilo uninstall` | quita el servicio, los hooks y la configuración; tus repos quedan como repos git normales |

`init`, `join` y `setup` vinculan la carpeta donde los corres, o `--folder DIR`. También aceptan
`--name NOMBRE` y `--no-service` (no instala launchd ni systemd).

## Día a día

| Comando | |
| --- | --- |
| `hilo status [--latency]` | máquinas conectadas, carpetas vinculadas, leases, conflictos, historial, aprobaciones pendientes, latencia p50/p95 |
| `hilo approve [RUTA \| --all \| --deny RUTA]` | revisa un cambio remoto a una ruta que ejecuta código |
| `hilo log RUTA` | todas las versiones de un archivo que guardó esta máquina, de la más nueva a la más vieja: cuándo, cómo (`local`, `remote`, `conflict`, `replaced`, `restored`), quién, tamaño |
| `hilo restore RUTA [--at N \| --conflict]` | trae de vuelta una versión: la más nueva que difiere del archivo, la línea `N` de `hilo log`, o la que perdió el último conflicto. Se sincroniza como una escritura nueva |
| `hilo trash [DIR]` | archivos borrados dentro de `DIR` (por defecto: todo el repo) que hilo puede traer de vuelta |
| `hilo run -- npm run dev` | corre con las variables, sin escribir ningún archivo |

## Variables de entorno

| Comando | |
| --- | --- |
| `hilo env set KEY=valor` · `unset KEY` | cambia una variable en todas partes (`--file apps/web/.env` en monorepos) |
| `hilo env local KEY` | cada máquina guarda su propio valor, por ejemplo `DATABASE_URL` |
| `hilo env prod KEY` | solo en esta máquina, y solo a través de `hilo run` |
| `hilo env ls` · `history [KEY]` · `revert KEY [N]` | capas, las últimas 50 versiones por archivo, volver atrás |

## Máquinas y acceso

| Comando | |
| --- | --- |
| `hilo invite` | un código de unión nuevo, de un solo uso (ancla) |
| `hilo unpair MÁQUINA` | desconecta una máquina y rota la llave de la bóveda (ancla) |
| `hilo trust MÁQUINA read-only\|full` | impide (o vuelve a permitir) que una máquina escriba aquí |
| `hilo token create [--env] [--addr HOST:PUERTO]` · `ls` · `revoke ID` | tokens reutilizables para sandboxes en la nube (ancla) |

## Configuración

`~/.hilo/config` es texto plano `clave = valor`. Casi nunca hace falta tocarlo.

| Clave | |
| --- | --- |
| `folder` | una carpeta vinculada (una línea por carpeta) |
| `mirrors` | dónde van las ramas espejadas: por defecto `<repo>.worktrees/<rama>` junto al repo; también una carpeta o `off` |
| `env = strict` | nunca escribe archivos `.env`; las variables solo llegan a `hilo run` |
| `readonly` | máquinas que no pueden escribir aquí (`hilo trust`) |
| `bind` | escucha en una dirección distinta de la que anuncia (NAT, contenedores) |
| `history_days` | cuánto tiempo quedan las versiones viejas en `~/.hilo/store` (por defecto `30`); la versión más nueva de cada archivo queda siempre |
| `history_max_mb` | lo máximo que puede ocupar el historial, en MB (por defecto `5120`); primero se van las versiones más viejas |

## Entorno

| Variable | |
| --- | --- |
| `HILO_HOME` | usa otra carpeta en vez de `~/.hilo` |
| `HILO_TOKEN` · `HILO_NAME` | token y nombre de máquina para `hilo up` |
| `HILO_PROXY` | proxy a usar, antes que `ALL_PROXY` y `HTTPS_PROXY` |
| `HILO_KEYCHAIN=0` | guarda las llaves en `~/.hilo/secrets` en vez del Keychain de macOS |
| `HILO_NOTIFY=0` | sin notificaciones de escritorio |
| `HILO_VERSION` · `HILO_BIN_DIR` | instalador: qué release (por defecto `latest`) y dónde dejarlo (por defecto `~/.local/bin`) |
| `HILO_REQUIRE_ATTESTATION=1` | instalador: rechaza una descarga sin una atestación de build válida de GitHub |
