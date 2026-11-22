#!/bin/sh
# hilo installer: downloads the release for this machine, checks its SHA-256 (and its GitHub
# build attestation when `gh` is available; HILO_REQUIRE_ATTESTATION=1 makes that mandatory),
# and puts `hilo` on your PATH.
set -eu
REPO="${HILO_REPO:-damkandev/hilo}"
VERSION="${HILO_VERSION:-latest}"
DEST="${HILO_BIN_DIR:-$HOME/.local/bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) T=aarch64-apple-darwin ;;
  Darwin-x86_64) T=x86_64-apple-darwin ;;
  Linux-x86_64) T=x86_64-unknown-linux-musl ;;
  Linux-aarch64|Linux-arm64) T=aarch64-unknown-linux-musl ;;
  *) echo "hilo: no build for $(uname -s) $(uname -m) yet" >&2; exit 1 ;;
esac

if [ "$VERSION" = latest ]; then BASE="https://github.com/$REPO/releases/latest/download"
else BASE="https://github.com/$REPO/releases/download/$VERSION"; fi

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
cd "$TMP"
curl -fsSLO "$BASE/hilo-$T.tar.gz"
curl -fsSLO "$BASE/SHA256SUMS"
if command -v sha256sum >/dev/null; then grep " hilo-$T.tar.gz\$" SHA256SUMS | sha256sum -c - >/dev/null
else grep " hilo-$T.tar.gz\$" SHA256SUMS | shasum -a 256 -c - >/dev/null; fi
echo "hilo: checksum ok"
# The checksum comes from the same release as the binary: it catches corruption, not a tampered
# release. The build attestation (signed by GitHub for this repo's workflow) is what proves origin.
if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
  # in an if, not after &&: set -e doesn't stop on a failed command inside an && list
  if gh attestation verify "hilo-$T.tar.gz" --repo "$REPO" >/dev/null; then
    echo "hilo: build attestation ok (built by $REPO's release workflow)"
  elif [ "${HILO_REQUIRE_ATTESTATION:-0}" = 1 ]; then
    echo "hilo: the build attestation of hilo-$T.tar.gz did not verify; not installing" >&2
    exit 1
  else
    echo "hilo: warning: the build attestation of hilo-$T.tar.gz did not verify (HILO_REQUIRE_ATTESTATION=1 refuses to install)" >&2
  fi
elif [ "${HILO_REQUIRE_ATTESTATION:-0}" = 1 ]; then
  echo "hilo: HILO_REQUIRE_ATTESTATION=1 needs the GitHub CLI (gh), logged in" >&2
  exit 1
else
  echo "hilo: note: build attestation not checked (no gh). To check it: gh attestation verify hilo-$T.tar.gz --repo $REPO"
fi
tar -xzf "hilo-$T.tar.gz"
mkdir -p "$DEST"
install -m 0755 hilo "$DEST/hilo"
echo "hilo: installed $DEST/hilo"
case ":$PATH:" in *":$DEST:"*) ;; *) echo "hilo: add $DEST to your PATH" ;; esac
# `curl … | sh -s -- up` installs and runs `hilo up` in one go (setup scripts of cloud agents)
if [ "$#" -gt 0 ]; then exec "$DEST/hilo" "$@"; fi
echo
echo "Now run: hilo"
