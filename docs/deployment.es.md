# Despliegue

<p align="center">
  <a href="deployment.md">English</a> · <b>Español</b>
</p>

Qué corre dónde, qué dirección usar y cómo se mantiene corriendo. El asistente (`hilo`) hace todo
esto en el caso común; esta página es para cuando quieras entenderlo o hacerlo a mano.

## Topología

```
  laptop     ──┐
               ├──►  ancla (servidor o VPS)  ◄──  sandbox en la nube
  escritorio ──┘
```

- **El ancla** es la única máquina que ordena los cambios. Elige una que esté siempre prendida y que
  las demás puedan alcanzar: un servidor en casa, una devbox o un VPS chico.
- **Todas las demás** abren una conexión saliente hacia ella. En tu casa no hace falta abrir puertos.
- **Sin atravesar NAT en la v1.** Una máquina que no llega a la dirección del ancla no puede unirse.

## La dirección del ancla

```sh
hilo init --listen 10.0.0.12:7710                              # la dirección que marcarán las demás
hilo init --listen 203.0.113.10:7710 --public                  # una dirección pública tiene que ser explícita
hilo init --listen 203.0.113.10:7710 --public --bind 0.0.0.0:7710   # detrás de NAT o en un contenedor
```

| Flag | |
| --- | --- |
| `--listen` | la IP y el puerto que marcan las demás. Por defecto, la primera IP privada de esta máquina en el puerto 7710. Nunca `0.0.0.0`. |
| `--public` | obligatorio para una IP pública. Los rangos privados, loopback y el `100.64.0.0/10` de Tailscale no lo necesitan. |
| `--bind` | dónde escucha de verdad el socket, cuando es distinto de la dirección anunciada (puerto redirigido, Docker). Se guarda como `bind` en `~/.hilo/config`. |

Antes de que un par se autentique, el ancla limita los frames a 1 MiB, le da 5 segundos al handshake
y acepta como mucho 16 conexiones pendientes.

| Escenario | Dirección del ancla |
| --- | --- |
| Mac y servidor en la misma red de casa | la IP de la red local del servidor |
| Máquinas en Tailscale o WireGuard | la IP del servidor en la tailnet |
| Sandboxes en la nube | la IP de un VPS con `--public`; los tokens pueden anunciar un dominio con `--addr` |

## Servicio en segundo plano

`hilo`, `setup`, `init` y `join` instalan un servicio por usuario que corre el daemon:

| Sistema | Servicio | Logs |
| --- | --- | --- |
| macOS | agente de launchd en `~/Library/LaunchAgents/`, arranca al iniciar sesión | `~/Library/Logs/hilo.log` |
| Linux | unidad de usuario de systemd `~/.config/systemd/user/hilo.service` | `journalctl --user -u hilo` |
| Windows | valor `hilo` en `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, arranca al iniciar sesión (sin admin) | `~/.hilo/daemon.log` |

En un servidor Linux, para que siga corriendo después de cerrar sesión:

```sh
sudo loginctl enable-linger $USER
```

Sin gestor de servicios (contenedores, sandboxes), `hilo up` arranca el daemon en segundo plano y
escribe en `~/.hilo/daemon.log`. `--no-service` omite el servicio en `setup`, `init` y `join`.

## Archivos

| Ruta | |
| --- | --- |
| `~/.hilo/config` | la configuración ([referencia](commands.es.md#configuración)) |
| `~/.hilo/key` · `peers` · `tokens` | la identidad de esta máquina, las llaves fijadas de los pares, los tokens de la nube |
| `~/.hilo/secrets` | Linux y Windows: las llaves que en macOS viven en el Keychain (permisos 0600; en Windows, la ACL de la carpeta del perfil) |
| `~/.hilo/store` | el historial de versiones detrás de `hilo log`, `restore` y `trash`, permisos 0600 (`history_days`, `history_max_mb`) |

`HILO_HOME` mueve todo eso. `hilo uninstall` quita el servicio, los hooks y la configuración; tus
repos quedan como repos git normales. También borra el historial de versiones del almacén.

## Plataformas

| | |
| --- | --- |
| macOS | Apple Silicon e Intel |
| Linux | x86_64 y aarch64, estático (musl): cualquier distro, incluidas Alpine y glibc viejas |
| Windows | 10 o posterior, x86_64; por ahora se compila desde el código (aún no hay binario en los releases) |

### Windows

```powershell
cargo build --release
copy target\release\hilo.exe $HOME\.local\bin\   # o cualquier carpeta de tu PATH
hilo
```

Lo que cambia respecto de macOS y Linux:

- El servicio es una clave Run del usuario, no un supervisor: arranca el daemon al iniciar sesión
  pero no lo reinicia si se cae, y no lleva `HILO_HOME` (defínelo como variable de entorno del usuario).
- La CLI habla con el daemon por TCP en loopback con un token por arranque en `~/.hilo/hilo.sock`,
  porque std no tiene sockets Unix en Windows.
- Usa `git config --global core.autocrlf false` (o `input`) en las máquinas Windows: hilo sincroniza
  los bytes del árbol de trabajo, así que un checkout con CRLF aparecería como cambios en las otras.
- Los archivos con nombres que Windows no admite (`aux.c`, `a:b`, un punto al final) se rechazan en Windows.
- `hilo run -- CMD` arranca `CMD` directamente: usa `npm.cmd`, o `cmd /c npm ...`, para scripts `.cmd`.
- Sin notificaciones de escritorio; quedan en el log.

## Actualizar

Corre el instalador de nuevo. Reemplaza el binario en `~/.local/bin` (o `HILO_BIN_DIR`); para fijar
un release usa `HILO_VERSION=v0.4.3`.
