# Graph Report - cloudfairy  (2026-11-18)

## Corpus Check
- Corpus is ~12,467 words - fits in a single context window. You may not need a graph.

## Summary
- 466 nodes · 813 edges · 19 communities (17 shown, 2 thin omitted)
- Extraction: 83% EXTRACTED · 17% INFERRED · 0% AMBIGUOUS · INFERRED: 137 edges (avg confidence: 0.86)
- Token cost: 324,713 input · 0 output

## Community Hubs (Navigation)
- Tunnel Process Runtime
- Frontend UI Logic
- Product Docs and Features
- cloudflared Binary Management
- Release Build Workflow
- Tauri Commands (main.rs)
- Tauri Config and Signing
- App Icons and Branding
- Settings and Events
- Pages Deploy Workflow
- Package Manifest
- CI Step Definitions
- In-App Updater
- Dark Mode Screenshot
- Light Mode Screenshot
- How It Works Diagram
- Tauri Capabilities
- Project Root

## God Nodes (most connected - your core abstractions)
1. `Cloudfairy App Icon` - 22 edges
2. `Tunnels` - 17 edges
3. `with` - 15 edges
4. `AppState` - 15 edges
5. `init()` - 15 edges
6. `CmdResult` - 14 edges
7. `Features` - 12 edges
8. `Cloudfairy Landing Page` - 12 edges
9. `errText()` - 11 edges
10. `refreshCloudflared()` - 11 edges

## Surprising Connections (you probably didn't know these)
- `Cloudfairy Watcher Panel (output -> card state)` --semantically_similar_to--> `Line-by-Line Output Parsing`  [INFERRED] [semantically similar]
  assets/readme/how-it-works.svg → README.md
- `Settings and cloudflared Storage Locations (dev.cloudfairy.desktop)` --references--> `managed_path()`  [INFERRED]
  README.md → src-tauri/src/cloudflared.rs
- `Live cloudflared Logs` --references--> `tunnel_logs()`  [INFERRED]
  README.md → src-tauri/src/main.rs
- `Protocol Setting (Auto / QUIC / HTTP2)` --references--> `Settings`  [INFERRED]
  README.md → src-tauri/src/settings.rs
- `cloudflared tunnel --no-autoupdate --url <target>` --references--> `build_args()`  [INFERRED]
  README.md → src-tauri/src/tunnel.rs

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **cloudflared Output Drives Tunnel Card State** — readme_log_line_parsing, readme_registered_tunnel_connection, readme_readable_errors, readme_local_server_check, assets_readme_how_it_works_watcher_panel, src_tauri_src_tunnel_tunnels_handle_line [INFERRED 0.85]
- **Cloudfairy Brand Identity Assets** — src_tauri_icons_app_icon_app_icon, site_favicon_favicon, site_og_image_social_card, assets_readme_hero_hero_banner, assets_readme_hero_fairy_gradient [INFERRED 0.85]
- **cloudflared Discovery and Download Flow** — ui_index_setup_banner, readme_cloudflared_discovery, ui_app_downloadcloudflared, src_tauri_src_cloudflared_resolve, src_tauri_src_cloudflared_download [INFERRED 0.85]
- **Tunnel creation and lifecycle flow** — assets_readme_app_dark_create_tunnel_form, assets_readme_app_dark_tunnel_card, assets_readme_app_dark_start_stop_control, assets_readme_app_dark_status_indicator, assets_readme_app_dark_public_url_panel [INFERRED 0.85]
- **Tunnel lifecycle: create, start, obtain public URL** — assets_readme_app_light_create_tunnel_form, assets_readme_app_light_tunnel_card, assets_readme_app_light_tunnel_actions, assets_readme_app_light_tunnel_status_indicator, assets_readme_app_light_public_url_bar [INFERRED 0.85]
- **Website favicon and touch icon set** — site_apple_touch_icon, site_favicon_32, site_icon_64, site_icon_192, site_icon_512 [INFERRED 0.85]
- **Tauri bundle icon set (desktop + Windows Store sizes)** — src_tauri_icons_icon, src_tauri_icons_128x128, src_tauri_icons_128x128_2x, src_tauri_icons_32x32, src_tauri_icons_64x64, src_tauri_icons_square107x107logo, src_tauri_icons_square142x142logo, src_tauri_icons_square150x150logo, src_tauri_icons_square284x284logo, src_tauri_icons_square30x30logo, src_tauri_icons_square310x310logo, src_tauri_icons_square44x44logo, src_tauri_icons_square71x71logo, src_tauri_icons_square89x89logo, src_tauri_icons_storelogo [INFERRED 0.85]

## Communities (19 total, 2 thin omitted)

### Community 0 - "Tunnel Process Runtime"
Cohesion: 0.06
Nodes (33): Cloudfairy Watcher Panel (output -> card state), 'Registered tunnel connection' -> Live, Tests (unit + quick_tunnel_end_to_end), TunnelConfig, build_args(), builds_args(), emit_update(), error_message() (+25 more)

### Community 1 - "Frontend UI Logic"
Cohesion: 0.10
Nodes (50): Any Local Address Input, In-App Self Update, appendLog(), cardFor(), cardTemplate(), checkAppUpdate(), checkLatest(), compareVersions() (+42 more)

### Community 2 - "Product Docs and Features"
Cohesion: 0.07
Nodes (40): Fairy Accent Gradient (#ff9a3c -> #e8467c), README Hero Banner, localhost:3000 to Public URL Transformation, Cloudfairy, Cloudflare 502 (Local Server Not Running), Cloudflare Quick Tunnel, cloudflared, ~/.cloudflared/config.yml Blocks Quick Tunnels (+32 more)

### Community 3 - "cloudflared Binary Management"
Cohesion: 0.09
Nodes (20): asset(), download(), exe_name(), extract_from_tgz(), fallback_locations(), http_client(), LATEST_CACHE, LATEST_TTL (+12 more)

### Community 4 - "Release Build Workflow"
Cohesion: 0.06
Nodes (32): env, name, needs, permissions, runs-on, strategy, concurrency, cancel-in-progress (+24 more)

### Community 5 - "Tauri Commands (main.rs)"
Cohesion: 0.18
Nodes (24): Copy / Open in Browser / QR Code, Info, info(), add_tunnel(), AppState, check_app_update(), cloudflared_info(), CmdResult (+16 more)

### Community 6 - "Tauri Config and Signing"
Cohesion: 0.06
Nodes (29): GITHUB_TOKEN, TAURI_SIGNING_PRIVATE_KEY, TAURI_SIGNING_PRIVATE_KEY_PASSWORD, env, app, security, windows, withGlobalTauri (+21 more)

### Community 7 - "App Icons and Branding"
Cohesion: 0.08
Nodes (27): Cloudfairy App Icon, Apple Touch Icon, Site Favicon 32x32, Site Icon 192x192, Site Icon 512x512, Site Icon 64x64, Social Share Card (og-image), Cloudfairy Brand Lockup (icon + wordmark) (+19 more)

### Community 8 - "Settings and Events"
Cohesion: 0.11
Nodes (12): Code Layout (ui invoke -> main.rs commands, events back), download-progress Event, settings.json Persistence, Settings and cloudflared Storage Locations (dev.cloudfairy.desktop), tunnel-log Event, Tunnels Saved Between Launches, tunnel-update Event, EVENT_DOWNLOAD (+4 more)

### Community 9 - "Pages Deploy Workflow"
Cohesion: 0.08
Nodes (25): concurrency, cancel-in-progress, group, environment, runs-on, steps, name, url (+17 more)

### Community 10 - "Package Manifest"
Cohesion: 0.09
Nodes (23): author, bugs, url, description, devDependencies, @tauri-apps/cli@^2.12.5, homepage, keywords (+15 more)

### Community 11 - "CI Step Definitions"
Cohesion: 0.11
Nodes (22): steps, if, name, run, shell, uses, with, working-directory (+14 more)

### Community 12 - "In-App Updater"
Cohesion: 0.31
Nodes (6): AppUpdate, check(), download(), EVENT_PROGRESS, PendingUpdate, Progress

### Community 13 - "Dark Mode Screenshot"
Cohesion: 0.23
Nodes (12): Cloudfairy Dark Mode Screenshot, cloudflared Version Status Pill, Create Tunnel Form, Dark Theme Design, App Header (Logo + cloudflared Version Badge + Settings), Local Server Not Responding Warning, Collapsible Logs + Edit/Remove Actions, Public trycloudflare.com URL Panel (+4 more)

### Community 14 - "Light Mode Screenshot"
Cohesion: 0.24
Nodes (12): Cloudfairy Light Mode Screenshot, cloudflared Version Status Pill, Create Tunnel Form, App Header (logo, cloudflared status, settings), Light Theme Visual Style, Local Server Not Answering Warning, Collapsible Logs Toggle, Public trycloudflare URL Bar (+4 more)

### Community 15 - "How It Works Diagram"
Cohesion: 0.39
Nodes (7): Anyone with the Link, Cloudflare Edge Network, cloudflared (started by Cloudfairy), How Cloudfairy Works Diagram, Your App (localhost:3000), cloudflared tunnel --no-autoupdate --url <target>, How It Works

### Community 16 - "Tauri Capabilities"
Cohesion: 0.33
Nodes (5): description, identifier, permissions, $schema, windows

## Knowledge Gaps
- **43 isolated node(s):** `cloudfairy`, `Custom`, `Managed`, `System`, `Stopped` (+38 more)
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 169 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)
- **2 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Cloudfairy` connect `Product Docs and Features` to `Package Manifest`, `How It Works Diagram`?**
  _High betweenness centrality (0.180) - this node is a cross-community bridge._
- **Are the 22 inferred relationships involving `Cloudfairy App Icon` (e.g. with `Apple Touch Icon` and `Site Favicon 32x32`) actually correct?**
  _`Cloudfairy App Icon` has 22 INFERRED edges - model-reasoned connections that need verification._
- **What connects `cloudfairy`, `Custom`, `Managed` to the rest of the system?**
  _43 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Tunnel Process Runtime` be split into smaller, more focused modules?**
  _Cohesion score 0.05874125874125874 - nodes in this community are weakly interconnected._
- **Why does `Code Layout (ui invoke -> main.rs commands, events back)` connect `Settings and Events` to `Tunnel Process Runtime`, `cloudflared Binary Management`, `Tauri Commands (main.rs)`, `Pages Deploy Workflow`, `How It Works Diagram`?**
  _High betweenness centrality (0.175) - this node is a cross-community bridge._
- **Are the 8 inferred relationships involving `init()` (e.g. with `downloadCloudflared()` and `installAppUpdate()`) actually correct?**
  _`init()` has 8 INFERRED edges - model-reasoned connections that need verification._
- **Should `Frontend UI Logic` be split into smaller, more focused modules?**
  _Cohesion score 0.09728506787330317 - nodes in this community are weakly interconnected._