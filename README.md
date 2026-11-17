# Cloudfairy

A desktop app for [Cloudflare quick tunnels](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/do-more-with-tunnels/trycloudflare/).
Type a local port and get a public `https://….trycloudflare.com` link, without running
`cloudflared tunnel --url http://localhost:3000` in a terminal and copying the URL out of its logs.

Built with Rust and [Tauri 2](https://v2.tauri.app). Runs on Windows, macOS and Linux.

## Features

- **Several tunnels at once.** Each one has its own Start/Stop button, status, and uptime.
- **Takes any local address:** `3000`, `:8080`, `localhost:5173`, `127.0.0.1:8000/app` or `https://localhost:8443`.
- **One-click Copy, Open in browser, and QR code** (for opening the link on your phone).
- **Live cloudflared logs** for each tunnel, with errors and warnings highlighted.
- **Checks your local server.** If nothing is listening on the port, a warning appears. It clears by itself once your server starts.
- **Readable errors.** Rate limits, crashes and a missing binary show up on the tunnel's card.
- **Finds or downloads cloudflared.** It checks your Settings path first, then a copy it downloaded, then your `PATH` and the usual install locations (Homebrew, winget, Program Files). If none is found, one click downloads the official release from GitHub.
- **Settings:** a custom cloudflared path, and the protocol (Auto / QUIC / HTTP2). Use HTTP2 on networks that block UDP.
- **Tunnels are saved between launches.** They don't start automatically. Every cloudflared process is stopped when you quit the app.

## Prerequisites

| | Needed |
|---|---|
| All | [Rust](https://rustup.rs) (stable) and [Node.js](https://nodejs.org) 18+ (only used to run the Tauri CLI) |
| Linux | `sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev librsvg2-dev build-essential` (or your distro's equivalents) |
| macOS | Xcode Command Line Tools: `xcode-select --install` |
| Windows | [Microsoft C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) ("Desktop development with C++"). WebView2 is already included in Windows 10/11. |

You don't need to install `cloudflared` beforehand. The app can download it for you.

## Run and build

```bash
npm install          # installs the Tauri CLI
npm run dev          # run the app in development mode
npm run build        # release build + installers in src-tauri/target/release/bundle/
```

`npm run build` makes installers for the OS you run it on:

- Linux: `.deb`, `.rpm`, `.AppImage`
- macOS: `.app`, `.dmg`
- Windows: `.msi`, NSIS `.exe`

Tauri doesn't cross-compile installers, so each one is built on its own OS. GitHub Actions
does this for you ([`.github/workflows/build.yml`](.github/workflows/build.yml)):

- **Every push to `main` and every pull request:** runs clippy and the unit tests on all three
  OSes. Then it builds Linux (x64), Windows (x64) and macOS (universal: Apple Silicon + Intel)
  installers and attaches them to the workflow run as artifacts.
- **Pushing a version tag** creates a draft GitHub Release with every installer attached:

  ```bash
  git tag v0.1.0 && git push origin v0.1.0
  ```

  Keep `version` in `src-tauri/tauri.conf.json` in step with the tag.

For apps you distribute, also set up code signing: Apple notarization on macOS, and a
signing certificate on Windows so SmartScreen doesn't warn users. See the Tauri distribution docs.

## Tests

```bash
cd src-tauri
cargo test                                         # unit tests: input parsing, log parsing, args
cargo test -- --ignored quick_tunnel_end_to_end    # real tunnel through Cloudflare (needs network + cloudflared on PATH)
```

The end-to-end test does the following:

1. Opens a tunnel to a port with nothing on it, and checks that the warning appears.
2. Starts a server on that port, and checks that the warning clears.
3. Fetches the public `trycloudflare.com` URL and checks that it reaches the local server.
4. Stops the tunnel.

## How it works

```
ui/ (HTML/CSS/JS) ──invoke──▶ src-tauri/src/main.rs (commands)
        ▲                          │
        └──── events ◀──── tunnel.rs: spawns `cloudflared tunnel --no-autoupdate --url <target>`
               tunnel-update         reads stdout/stderr line by line:
               tunnel-log              • URL in the banner box         → publicUrl
               download-progress       • "Registered tunnel connection" → status "running"
                                       • "ERR …"                        → error message
                                     polls the child process for exit → "stopped" / "error"
```

| File | Purpose |
|---|---|
| `src-tauri/src/tunnel.rs` | Process lifecycle, log parsing, local-port check, input normalization |
| `src-tauri/src/cloudflared.rs` | Finding the binary, reading its version, downloading from GitHub releases (`.tgz` on macOS) |
| `src-tauri/src/settings.rs` | `settings.json` persistence |
| `src-tauri/src/main.rs` | Tauri commands and app setup; kills all tunnels on exit |
| `ui/` | The interface. Plain HTML/CSS/JS, no bundler. Follows the system light/dark theme. |

Where files are stored:

| | Settings | Downloaded `cloudflared` |
|---|---|---|
| Linux | `~/.config/dev.cloudfairy.desktop/` | `~/.local/share/dev.cloudfairy.desktop/bin/` |
| macOS | `~/Library/Application Support/dev.cloudfairy.desktop/` | same folder, `bin/` |
| Windows | `%APPDATA%\dev.cloudfairy.desktop\` | same folder, `bin\` |

## Troubleshooting

- **"429 Too Many Requests"**: Cloudflare limits how often quick tunnels can be created. Wait a minute and press Retry.
- **The tunnel fails right away and you have `~/.cloudflared/config.yml`**: cloudflared doesn't support quick tunnels while a config file exists. Rename the file while you use quick tunnels.
- **It never reaches "Live" on an office or school network**: set the protocol to HTTP/2 in Settings.
- **The URL shows a Cloudflare 502 page**: your local server isn't running, or it's on a different port.
- **Vite / webpack dev server says "host not allowed"**: add `.trycloudflare.com` to the dev server's allowed hosts. For Vite, that's `server.allowedHosts`.

Quick tunnels are meant for testing and sharing. They have no uptime guarantee, and the URL changes every time you start one.
