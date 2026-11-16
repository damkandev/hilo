# Seguridad

<p align="center">
  <a href="SECURITY.md">English</a> · <b>Español</b>
</p>

## Reportar una vulnerabilidad

Por favor no abras un issue público. Repórtala en privado por GitHub:
**Security → Report a vulnerability** en
[damkandev/hilo](https://github.com/damkandev/hilo/security/advisories/new).

## Estado

**hilo no tiene auditoría externa, y `snow` tampoco: lo dice su README.** Lee lo que sigue como una
descripción del diseño y de sus tests, no como una garantía.

## Red

- **Emparejamiento:** un código de un solo uso y 10 minutos, mezclado en un handshake `Noise_XXpsk3_25519_ChaChaPoly_SHA256` (lo ejecuta `snow`) probado contra los vectores oficiales de cacophony. Después solo se conectan llaves conocidas, y cada lado fija la llave del otro.
- **El ancla** escucha solo en la dirección privada que elijas. Una pública exige `--public`. Antes de que un par se autentique: tope de 1 MiB por frame, 5 segundos para el handshake y como mucho 16 conexiones pendientes.

## Archivos

- **Cada máquina valida cada ruta que recibe.** Rechaza `.git` con cualquier mayúscula o disfraz Unicode, abre cada carpeta con `openat(O_NOFOLLOW)`, crea los temporales con `O_EXCL`, nunca aplica setuid y rechaza nombres que chocan en un disco que no distingue mayúsculas.
- **Las rutas que ejecutan código** (`.claude/`, `.mcp.json`, `.envrc`, `.vscode/`, `.husky/`, `.github/workflows/`, `Makefile`, los `scripts` de `package.json`, `CLAUDE.md`, `AGENTS.md`) esperan a `hilo approve`, que muestra el diff.
- **Los secretos nunca se sincronizan como archivos.** Eso incluye archivos de llaves y formatos de token conocidos (AWS, Stripe live, GitHub, llaves privadas).

## Llaves y envs

- **Las llaves viven en el Keychain de macOS,** y solo el binario firmado de hilo puede leerlas. En Linux viven en `~/.hilo/secrets` con permisos 0600. El socket local nunca devuelve el valor de un secreto.
- **Los valores de `.env`** viven en una bóveda por repo (ChaCha20-Poly1305). El archivo `.env` escrito es solo una barrera para los agentes con hooks; mira [Envs](docs/architecture.es.md#envs) para `env = strict` y `hilo env prod`.

## Criptografía

Ninguna primitiva ni máquina de estados de protocolo está escrita en este repo. El handshake y el
transporte de Noise corren sobre `snow`; X25519, ChaCha20-Poly1305, SHA-256 y HKDF vienen de
`x25519-dalek`/`curve25519-dalek` y de RustCrypto (`chacha20poly1305`, `sha2`, `hkdf`), todos fijados
y vendorizados. Encima, hilo rechaza llaves públicas de orden bajo (`snow` no lo hace), y el ancla
prueba cada código de emparejamiento candidato repitiendo su lado del handshake.

Los tests verifican los crates contra FIPS 180-4, RFC 4231, RFC 5869 y el vector de cacophony. Tests
de fuzzing deterministas comprueban que se rechazan mensajes del handshake y del transporte
alterados, truncados, reordenados o repetidos, que quedarse sin nonces es un error y no una
reutilización, y que el decodificador resiste 200.000 entradas mutadas.

**`unsafe`** solo aparece en los bindings del sistema (FSEvents, inotify, el Keychain, `openat`). Todo
módulo que procesa datos de la red es `#![forbid(unsafe_code)]`.

## Cadena de suministro

`cargo vet` pasa. `sha2`, `hmac`, `subtle`, `zeroize` y algunas piezas más chicas están cubiertas por
auditorías importadas de otras organizaciones (ISRG entre ellas; ver `supply-chain/imports.lock`). El
resto de los crates de cripto pasan como excepciones explícitas, sin auditar en este repo:

| Crate | Auditoría |
| --- | --- |
| `snow` | ninguna formal, según su README |
| `x25519-dalek`, `curve25519-dalek` | no documentan ninguna |
| `chacha20poly1305`, `poly1305` | NCC Group, 2020, según su README |
| `hkdf`, `digest` | exentos |

`supply-chain/config.toml` dice cuál es cuál. La CI compila con `--offline --locked` desde los
crates vendorizados y corre `cargo vet --locked`.

## Verificar una descarga

El instalador revisa el SHA-256, que detecta una descarga corrupta pero viene del mismo release que
el binario. La prueba de origen es la atestación de build de GitHub: el instalador la revisa si `gh`
tiene sesión, y `HILO_REQUIRE_ATTESTATION=1` la vuelve obligatoria. A mano:

```sh
gh attestation verify hilo-<target>.tar.gz --repo damkandev/hilo
```
