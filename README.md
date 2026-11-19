# `@shinick/sharepoint` (`sharepoint` / `sp` / `onedrive` / `od`)

Unified **Microsoft SharePoint & OneDrive** native terminal client and AI agent toolkit built in Rust — sharing **1 binary, 1 MS Graph OAuth2 session, 1 SQLite WAL+FTS5 cache, 1 IPC sync daemon, 1 HTTP API (`8790`), and 1 MCP server (16 tools)**.

## Dual-Persona Launchers

| Command | Persona | Default Boot Target |
| --- | --- | --- |
| `sharepoint` / `sp` | SharePoint | SharePoint Sites & Document Libraries |
| `onedrive` / `od` | OneDrive (`SHAREPOINT_PERSONA=onedrive`) | Personal OneDrive (`My Files`, `Recent`, `Shared with Me`) |

## Install

```bash
npm install -g @shinick/sharepoint
```

Or build from source:

```bash
cargo build --release
```

## Quick Start

```bash
# Sign in (PKCE loopback, --device-code, --client-secret, or shared teams-cli M365 session)
sharepoint login
sharepoint whoami --json

# Launch 3-pane Minimal-Chrome TUI
sharepoint          # SharePoint Sites & Document Libraries
onedrive            # Personal OneDrive

# Browse & manage files
sharepoint sites --json
sharepoint drives --site Engineering --json
onedrive ls /Projects --json
onedrive tree --depth 2
onedrive cat /Projects/README.md
onedrive download /Projects/diagram.png --dir ./downloads --json
onedrive upload ./report.pdf /Projects --json
onedrive mkdir "Q4 Plans" --json
onedrive mv /Projects/draft.md --name final.md --json
onedrive rm /Projects/old.txt --yes --json
sharepoint search "roadmap" --remote --json
sharepoint share /Projects/final.md --type view --scope organization --json

# Agent interfaces & daemon
sharepoint serve --port 8790
sharepoint mcp
sharepoint daemon start
```

## Interactive 3-Pane TUI Highlights

- **Minimal-Chrome 3-pane layout** with 5 color palettes (`Auto`, `Midnight Dark`, `Daylight Light`, `ANSI 256`, `ANSI 16`, `No Color`)
- **Persona-aware collapsible sidebar**: `OneDrive` (`My Files`, `Recent`, `Shared with Me`) + `SharePoint` (Followed & Discovered Sites -> Document Libraries) + `Pinned`
- **Live inspector pane**: metadata, sharing status, version history, **inline terminal image preview** (80ms CSI/DA1/Kitty probe + Lanczos3 + Sixel Floyd-Steinberg dithering + `v`/`Alt+v` HD overlay), and **text/Markdown/JSON/YAML/CSV/code preview**
- **`Ctrl+V` Clipboard PNG Paste**: immediately encodes clipboard image as PNG and uploads `pasted-image-<unix-ms>.png` into the current folder
- **`Ctrl+F` File Picker (`z` zoxide jump)**: browse local folders and upload directly into the active OneDrive/SharePoint folder
- **`u` Link Extractor & `h` href toggle**, `d` download, `n` new folder, `r` rename, `x`/`Del` delete with confirm, `s` share link, `o` open in browser, `/` hybrid FTS5+Graph search, `Ctrl+P`/`Ctrl+K` Command Palette, `Ctrl+,` Settings, `.`/right-click Context Menu
