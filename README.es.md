<p align="center">
  <img src="assets/banner.es.png" width="100%" alt="hilo. El mismo código, vivo en todas tus máquinas. Sin push, sin pull y sin copiar archivos .env de un lado a otro.">
</p>

<p align="center">
  <a href="https://github.com/damkandev/hilo/actions/workflows/ci.yml"><img src="https://github.com/damkandev/hilo/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/damkandev/hilo/releases/latest"><img src="https://img.shields.io/github/v/release/damkandev/hilo" alt="Último release"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey" alt="macOS y Linux">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="Licencia MIT"></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <b>Español</b>
</p>

<p align="center">
  <img src="assets/wizard.svg" width="678" alt="El asistente de hilo emparejando un Mac con un servidor llamado devbox">
</p>

Un agente en tu Mac y otro en tu servidor trabajan cada uno sobre su propia copia del repo. Lo que
cambia uno, el otro no lo ve hasta que alguien hace commit, push y pull. Mientras tanto se pisan,
trabajan sobre código viejo y cada `.env` hay que copiarlo a mano a cada máquina y cada worktree.

hilo vive debajo de tus agentes (Claude Code, Codex, T3 Code, Cursor, OpenCode, cualquier cosa que
escriba archivos) y mantiene las máquinas al día a nivel de archivos y git.

## Para empezar

```sh
curl -fsSL https://raw.githubusercontent.com/damkandev/hilo/main/install.sh | sh
hilo
```

`hilo` te pregunta qué carpeta mantener sincronizada y dónde está tu otra máquina (te muestra los
hosts de `~/.ssh/config`). Después hace el resto por SSH:

1. instala hilo en la otra máquina y la deja como **ancla**
2. empareja las dos máquinas (el código de unión viaja por SSH; nunca lo copias)
3. vincula la carpeta en los dos lados y espera a que todos los repos estén al día
4. instala un servicio en segundo plano, los hooks de Claude Code y el servidor MCP, en ambas

Desde ahí, cada repo de esa carpeta se sincroniza, incluidos los que crees después.

<details>
<summary>Sin el asistente</summary>

```sh
hilo setup devbox --folder ~/code            # lo mismo, sin preguntas

# o a mano, desde la carpeta que quieres vincular:
hilo init --listen 10.0.0.12:7710      # en la máquina que las demás pueden alcanzar
hilo join HILO-7KQ2-M9PA-XR4T@10.0.0.12:7710   # en cada una de las otras
```

Mira [Despliegue](docs/deployment.es.md) para direcciones, servicios y plataformas.

</details>

## Qué obtienes

| | |
| --- | --- |
| **Cada escritura, en todas partes** | Un archivo que escribe un agente aparece en la otra máquina en unos 50 ms, con una Wi-Fi de casa. |
| **No se pierde nada** | Si dos máquinas cambian el mismo archivo a la vez, una gana y la versión de la otra queda guardada en una rama `hilo/conflict/<rama>/<fecha>`. |
| **Los agentes se enteran antes** | Antes de que un agente edite un archivo que otro está editando, Claude Code bloquea la edición y le dice quién lo tiene, dónde y por cuánto tiempo. |
| **Ramas y commits** | Los commits viajan con los archivos, las ramas avanzan en todas las máquinas y un worktree nuevo en una aparece en las demás. |
| **Repos nuevos, solos** | Haz `git init` en una carpeta vinculada, el primer commit, y el repo aparece en la otra máquina con su historial y sus archivos sin commitear. |
| **`.env` sin copiar** | Las variables viajan cifradas, clave por clave, y se escriben en cada worktree que las necesita. Una clave puede quedar por máquina o solo en producción. |
| **También agentes en la nube** | Los sandboxes de Claude Code en la web, Codex, Cursor y Codespaces se unen con un token, y su rama aparece en tu Mac mientras trabajan. [Cómo →](docs/cloud-agents.es.md) |

## Cómo funciona

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/how-dark.es.svg">
    <img src="assets/how-light.es.svg" width="760" alt="Un Mac y un servidor; el servidor corre el ancla que ordena cada cambio">
  </picture>
</p>

Una máquina, el **ancla**, le pone un número de secuencia a cada cambio; las demás se conectan a
ella con una conexión saliente y cifrada. Cada escritura lleva el hash de la versión sobre la que se
hizo, y el ancla acepta solo la primera escritura sobre cada versión. La edición que pierde nunca se
mezcla ni se descarta: queda guardada en una rama de conflicto. [Más detalle →](docs/architecture.es.md)

## Documentación

| | |
| --- | --- |
| [Arquitectura](docs/architecture.es.md) | el ancla, la regla de escritura, ramas vivas, leases, la bóveda de `.env` |
| [Agentes en la nube](docs/cloud-agents.es.md) | tokens, `hilo up`, proxies, qué puede ver un sandbox |
| [Comandos y configuración](docs/commands.es.md) | cada comando, `~/.hilo/config`, variables de entorno |
| [Despliegue](docs/deployment.es.md) | elegir el ancla, direcciones, servicios en segundo plano, archivos, plataformas |
| [Seguridad](SECURITY.es.md) | red, archivos, llaves, criptografía, cadena de suministro, verificar descargas, reportes |
| [Contribuir](CONTRIBUTING.es.md) | compilar, testear, CI y releases |

## Estado

hilo es joven (v0.3) y no tiene auditoría de seguridad externa. Un solo binario de Rust de unos
1,6 MB; los únicos crates son de criptografía.

**Fuera de la v1:** Windows, atravesar NAT (el ancla necesita una dirección que las demás puedan
alcanzar) y sincronizar la conversación del agente.

## Licencia

[MIT](LICENSE)
