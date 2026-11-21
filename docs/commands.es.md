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
| `hilo add DIR` | vincula otra carpeta de repos, o un solo repo. Una carpeta con archivos y sin repos adentro queda como [drive](architecture.es.md#carpetas-drive): sus archivos se sincronizan tal cual. Una carpeta vacía queda como carpeta que recibe repos |
| `hilo add --drive DIR [--as ETIQUETA]` | vincula `DIR` como drive, vacía o con repos adentro (se saltan; `hilo add` sobre uno lo sincroniza como repo). Las máquinas reconocen un drive por `ETIQUETA`, por defecto el nombre de la carpeta en minúsculas; `--as` solo también significa drive. Sobre un drive cuya carpeta se desmontó o se reemplazó, deja como drive la carpeta que está ahí ahora |
| `hilo up [--read-only] [--drive ETIQUETA]` | en el script de setup de un sandbox: une, vincula, arranca y espera a quedar al día ([Agentes en la nube](cloud-agents.es.md)). `--drive ETIQUETA` vincula la carpeta (la crea si no existe) como ese drive |
| `hilo remove DIR` | deja de sincronizar aquí una carpeta, repo o drive vinculado; sus archivos se quedan donde están |
| `hilo uninstall` | quita el servicio, los hooks y la configuración; tus repos quedan como repos git normales |

`init`, `join` y `setup` vinculan la carpeta donde los corres, o `--folder DIR`. También aceptan
`--as ETIQUETA` (la vincula como ese drive; `setup` lo hace en las dos máquinas), `--name NOMBRE` y
`--no-service` (no instala launchd ni systemd). `hilo setup --hosts` lista los hosts de
`~/.ssh/config`. `status`, `log RUTA` y `trash DIR` aceptan `--json`, para la app de Mac.

## Día a día

| Comando | |
| --- | --- |
| `hilo status [--latency]` | máquinas conectadas, carpetas vinculadas, leases, conflictos, historial, aprobaciones pendientes, latencia p50/p95; en un sandbox con un token con alcance, ese alcance y los archivos que se quedan ahí por estar fuera de él |
| `hilo approve [RUTA \| --all \| --deny RUTA]` | revisa un cambio remoto a una ruta que ejecuta código |
| `hilo log RUTA` | todas las versiones de un archivo que guardó esta máquina, de la más nueva a la más vieja: cuándo, cómo (`local`, `remote`, `conflict`, `replaced`, `restored`), quién, tamaño |
| `hilo restore RUTA [--at N \| --conflict]` | trae de vuelta una versión: la más nueva que difiere del archivo, la línea `N` de `hilo log`, o la que perdió el último conflicto. Se sincroniza como una escritura nueva |
| `hilo trash [DIR]` | archivos borrados dentro de `DIR` (por defecto: todo el repo) que hilo puede traer de vuelta |

El historial empieza con la primera versión que hilo ve después de actualizar: un archivo que nadie
ha tocado desde entonces todavía no tiene historial en esta máquina. Si borras uno de esos archivos
aquí, las otras máquinas lo conservan, porque el borrado les llega como una versión `replaced`.
| `hilo run -- npm run dev` | corre con las variables, sin escribir ningún archivo |
| `hilo grep PATRÓN [DIR] [-i]` | búsqueda literal en los archivos sincronizados bajo `DIR` (por defecto, aquí); los ignorados no se buscan |

## Agentes por MCP

`hilo mcp` es un servidor MCP por stdio para agentes sin hooks, y para sandboxes que solo deben tocar
archivos sincronizados. Las rutas son absolutas o relativas al directorio de trabajo del agente, y
tienen que estar dentro de una carpeta sincronizada; cualquier otra se rechaza. Sin daemon, las
herramientas de archivos responden "hilo daemon is not running".

| Herramienta | |
| --- | --- |
| `claim` · `release` · `who` | toma un archivo antes de editarlo, lo suelta, muestra quién lo tiene |
| `read` `path` `offset?` `limit?` | el texto de un archivo, cortado en 256 KB (`offset`/`limit` en líneas); los binarios responden con su tamaño |
| `write` `path` `content` | toma el archivo, lo escribe entero y de forma atómica, y lo sincroniza al tiro; se rechaza si otro agente lo tiene, o con un token de solo lectura |
| `list` `path?` `depth?` | archivos y carpetas con su tamaño, `depth` de 1 a 4 |
| `search` `pattern` `path?` `ignore_case?` | igual que `hilo grep`: `ruta:línea: texto`, máximo 200 resultados |

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
| `hilo token create [--env] [--addr HOST:PUERTO] [--path P] [--expires 24h] [--read-only]` · `ls` · `revoke ID` | tokens reutilizables para sandboxes en la nube, si quieres limitados a una carpeta o un repo, por un rato, sin escribir (ancla) |

## Configuración

`~/.hilo/config` es texto plano `clave = valor`. Casi nunca hace falta tocarlo.

| Clave | |
| --- | --- |
| `folder` | una carpeta vinculada (una línea por carpeta) |
| `drive` | un drive: `drive = <etiqueta> <ruta absoluta>`, la ruta puede tener espacios |
| `mirrors` | dónde van las ramas espejadas: por defecto `<repo>.worktrees/<rama>` junto al repo; también una carpeta o `off` |
| `env = strict` | nunca escribe archivos `.env`; las variables solo llegan a `hilo run` |
| `readonly` | máquinas que no pueden escribir aquí (`hilo trust`) |
| `bind` | escucha en una dirección distinta de la que anuncia (NAT, contenedores) |
| `history_days` | cuánto tiempo quedan las versiones viejas en `~/.hilo/store` (por defecto `30`); la versión más nueva de cada archivo queda siempre |
| `history_max_mb` | lo máximo que puede ocupar el historial, en MB (por defecto `5120`); primero se van las versiones más viejas |
| `scope_ro = 1` | este sandbox entró con un token de solo lectura: sus propias herramientas rechazan escrituras (`hilo up --read-only`) |

## Entorno

| Variable | |
| --- | --- |
| `HILO_HOME` | usa otra carpeta en vez de `~/.hilo` |
| `HILO_TOKEN` · `HILO_NAME` · `HILO_READONLY=1` | token, nombre de máquina y solo lectura para `hilo up` |
| `HILO_PROXY` | proxy a usar, antes que `ALL_PROXY` y `HTTPS_PROXY` |
| `HILO_KEYCHAIN=0` | guarda las llaves en `~/.hilo/secrets` en vez del Keychain de macOS |
| `HILO_NOTIFY=0` | sin notificaciones de escritorio |
| `HILO_VERSION` · `HILO_BIN_DIR` | instalador: qué release (por defecto `latest`) y dónde dejarlo (por defecto `~/.local/bin`) |
| `HILO_REQUIRE_ATTESTATION=1` | instalador: rechaza una descarga sin una atestación de build válida de GitHub |
