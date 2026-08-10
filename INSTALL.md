# Installing Pkzz

Pkzz has four installable pieces. Most people want exactly two: a **relay**
(the server that hosts a community) and a **client** (desktop and/or mobile)
that connects to it.

| Piece | Install path | Build required? |
|---|---|---|
| Relay (server) | Docker Compose bundle | No — prebuilt public image |
| Desktop app | Source build | Yes (tagged installer releases planned) |
| Mobile app | Source build | Yes |
| `buzz` CLI | Cargo | Yes |

> **Status honesty:** there are no published desktop/mobile installers yet.
> The release pipeline (`.github/workflows/release.yml`) produces them when a
> `desktop-v<version>` tag is pushed, but no tag has been cut. Until then the
> desktop and mobile apps build from source. The relay needs no build.

---

## 1. Relay (server) — Docker, no build

The relay image is published publicly from `main`:

```
ghcr.io/kingkillery/pkzz:main
```

Single-node/VPS deployment (Postgres, Redis, MinIO, relay, optional Caddy TLS):

```bash
git clone https://github.com/kingkillery/pkzz.git
cd pkzz/deploy/compose
cp .env.example .env
$EDITOR .env       # replace every CHANGE_ME value
./run.sh start
```

For a public host with automatic Let's Encrypt certificates:

```bash
BUZZ_COMPOSE_TLS=true ./run.sh start
```

Requires Docker Compose v2.24.4+. Full operator notes (secrets to keep stable,
closed relay mode, migrations):
[deploy/compose/README.md](deploy/compose/README.md).

### Private-network (Tailscale) install

If the relay is for you and your devices only, skip Caddy and DNS: bind the
compose stack on a tailnet node and run `tailscale serve` there. Clients then
use `wss://<host>.<tailnet>.ts.net` — real TLS, tailnet-only, and it satisfies
mobile platforms' cleartext-transport bans (plain `ws://` to a `100.x` address
is blocked by Android/iOS).

### Relay from source (development)

```bash
. ./bin/activate-hermit        # repo toolchain (Rust, Node, ...)
cp .env.example .env
just setup                     # deps + migrations
just relay                     # ws://localhost:3000
```

---

## 2. Desktop app — from source

Tauri 2 + React. Until tagged installer releases exist, build locally:

```bash
. ./bin/activate-hermit
just setup
just dev           # full native app
# or: just desktop-dev   (web-only dev server, faster iteration)
```

Signed internal builds (Block) come from a separate private pipeline — see
[RELEASING.md](RELEASING.md). Public installers ride the `desktop-v*` tag
flow in the same document; watch the
[releases page](https://github.com/kingkillery/pkzz/releases).

On first launch the app asks for a **community relay URL** — use the relay
from step 1 (`wss://your-host` or `ws://localhost:3000` for local dev).

## 3. Mobile app — from source

Flutter (Riverpod). Requires the Flutter SDK; iOS builds need Xcode, Android
builds need the Android SDK.

```bash
cd mobile
flutter pub get
flutter test          # verify the build works
```

`just mobile-dev` from the repo root boots Docker, a local relay, and an iOS
simulator in one step. To use your own relay instead of the local one, add a
community with your relay URL in the app's connection settings — see
[mobile/README.md](mobile/README.md). Device pairing (NIP-AB) can transfer
your desktop identity to the phone instead of typing keys.

## 4. `buzz` CLI — cargo

The agent-first CLI used by scripts and agents:

```bash
cargo build --release -p buzz-cli
# binary: ./target/release/buzz
```

Point it at a relay with `BUZZ_RELAY_URL` + `BUZZ_PRIVATE_KEY` (see
[.env.example](.env.example)). Full command surface:
`./target/release/buzz --help`.

---

## Verifying an install

```bash
# relay health + metadata
curl -s http://localhost:8080/health            # health router port (8080 in compose)
curl -s -H "Accept: application/nostr+json" http://localhost:3000/

# CLI end-to-end
buzz --format compact channels list
```

## System requirements

- **Relay:** Docker Compose v2.24.4+ (bundle), or Rust stable + Postgres 15+ /
  Redis 7+ (source). ~1 GB RAM idle.
- **Desktop:** Node/pnpm + Rust stable (Hermit provisions both via
  `. ./bin/activate-hermit`), platform webview (WebView2 on Windows,
  WebKitGTK on Linux).
- **Mobile:** Flutter with Dart ^3.11 (per `mobile/pubspec.yaml`), Xcode
  for iOS builds, Android SDK for Android builds.
