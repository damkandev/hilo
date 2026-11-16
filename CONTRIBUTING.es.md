# Contribuir

<p align="center">
  <a href="CONTRIBUTING.md">English</a> · <b>Español</b>
</p>

## Compilar y testear

```sh
HILO_KEYCHAIN=0 HILO_NOTIFY=0 cargo test --offline   # incluye la simulación de 400.000 pasos
cargo test -- --ignored keychain                      # usa el Keychain real de tu sesión
cargo vet
```

Todos los crates están vendorizados en `vendor/`, así que la compilación nunca toca la red.

## Reglas del código

- **Solo la criptografía viene de crates.** JSON, el formato de red, la interfaz de terminal y los
  watchers de archivos están escritos sobre la biblioteca estándar y llamadas al sistema. Una
  dependencia nueva necesita una muy buena razón, una versión fijada con `=`, una copia vendorizada y
  una entrada en `cargo vet`.
- **Nada de cripto escrita a mano.** Las primitivas y las máquinas de estados de protocolo vienen de
  `snow`, dalek y RustCrypto.
- **`unsafe` se queda en los bindings del sistema.** Los módulos que procesan datos de la red son
  `#![forbid(unsafe_code)]`.
- **Los cambios de sincronización pasan por la simulación.** Todo lo que toque el orden, los
  conflictos o el formato de red tiene que mantener en verde la simulación de 400.000 pasos.

Los problemas de seguridad van por [SECURITY.es.md](SECURITY.es.md), no por issues públicos.

## CI

[`ci.yml`](.github/workflows/ci.yml) corre en cada push a `main` y en cada pull request:

- `cargo test --release --offline --locked` en macOS, Linux x86_64 y Linux arm64
- `cargo vet --locked`

## Releases

Hacer push de un tag `v*` corre [`release.yml`](.github/workflows/release.yml):

1. compila `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` y
   `aarch64-unknown-linux-musl`
2. firma y notariza los binarios de macOS cuando están los secretos de Apple
3. atesta cada tarball y `SHA256SUMS` con la procedencia de build de GitHub
4. publica un release de GitHub, que es lo que descarga [`install.sh`](install.sh)

Sube `version` en `Cargo.toml` antes de crear el tag.
