# Agentes en la nube

<p align="center">
  <a href="cloud-agents.md">English</a> · <b>Español</b>
</p>

Los agentes que corren en el sandbox de otro (Claude Code en la web, Codex, los background agents de
Cursor, Codespaces, Gitpod, Coder, dev containers) también pueden unirse. Cada sandbox entra como una
máquina de vida corta, y la rama donde trabaja aparece en tu Mac **mientras trabaja**, antes de
cualquier push.

## Configuración

En el ancla, crea un token (o corre `hilo` y elige **Let cloud agents in**):

```sh
hilo token create --addr hilo.example.com:7710        # con --env también reciben los valores de .env
```

Después, en la plataforma:

1. agrega el token como secreto: `HILO_TOKEN=HILO-CLOUD-…@hilo.example.com:7710`
2. agrega una línea a su script de setup:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/damkandev/hilo/main/install.sh | sh -s -- up
   ```

`hilo up` se une con el token, vincula el repo donde se ejecuta, arranca el daemon en segundo plano
(sin systemd ni launchd) y espera a quedar sincronizado. `HILO_NAME` define el nombre de la máquina
(por defecto `cloud-<host>`).

## Proxies

Los sandboxes que solo salen a internet a través de un proxy funcionan tal cual: hilo pasa por
`HILO_PROXY`, `ALL_PROXY` o `HTTPS_PROXY` (HTTP CONNECT o SOCKS5) y respeta `NO_PROXY`, así que
Tailscale en modo userspace también sirve.

## Lo que hay que saber

| | |
| --- | --- |
| **Alcance** | El sandbox tiene que llegar al ancla: usa un VPS como ancla, un puerto abierto en el router o un dominio, y permítelo en la configuración de red de la plataforma. Si la plataforma corta la red al terminar el setup, hilo solo sincroniza durante el setup. Mira [Despliegue](deployment.es.md). |
| **Mismo repo, cualquier clon** | Un repo es su primer commit más `dueño/repo`, así que un clon hecho a través del proxy de git de la plataforma sigue siendo el mismo repo. |
| **Secretos** | Sin `--env`, un sandbox nunca recibe las llaves de la bóveda. Con `--env` recibe la capa compartida; las claves de `hilo env prod` nunca salen de tu máquina. |
| **Limpieza** | Los sandboxes que no se conectan en 7 días se olvidan. `hilo token revoke ID` corta de una vez a todos los que usaron ese token. |
