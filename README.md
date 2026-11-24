# whity-agent

Whity's desktop companion. It finds the AI tools on your computer (Claude Code, Codex CLI, Cursor, VS Code, Gemini CLI and others), connects them to your Whity workspace over MCP, and lets you chat with Whity.

Where it is going: [ADR 0001, Whity Desktop foundations](docs/adr/0001-whity-desktop-foundations.md). Design: [Agent personas, presets and whity-agent](https://github.com/whity-org/whity-core/blob/develop/docs/design/AGENT-PERSONAS-AND-WHITY-AGENT.md) in whity-core, part B. Tasks: WHIT-822 to WHIT-824 (Taskaat, section "Agents").

## What works today

- **Sign in to a workspace.** Type its address (`app.whity.dev`). The app reads the workspace's Protected Resource Metadata, finds its authorization server (Whity ID), uses whity-agent's published client document at Whity ID (`https://id.whity.dev/clients/whity-agent.json`), or registers once as a public client at another authorization server, and signs you in in your browser with OAuth 2.1 + PKCE and a one-time loopback redirect on `127.0.0.1`.
- **Tokens stay out of reach.** Every network call and token lives in Rust (`src-tauri/src`). The webview only sees the workspace's name and address. Refresh tokens go to the OS keychain (Windows Credential Manager, macOS Keychain, Linux Secret Service) and nowhere else; with no keychain, the sign-in lasts until the app closes.
- **Find and connect your AI tools.** Claude Code, Codex CLI, Gemini CLI, VS Code, Cursor, Windsurf and Claude Desktop are found by looking at PATH and their config files only, never by running them. Connecting shows exactly what will happen first (the tool's own command, or the file before and after) and does nothing until you confirm. Tools get the workspace's URL and sign in themselves; no token is ever written. File edits touch only our entry, refuse a file that changed since you reviewed it or isn't plain JSON, keep a backup, write atomically, and can be undone (`src-tauri/src/tools.rs`, `config_writer.rs`, `linking.rs`). The tool list mirrors `local` in whity-core's `@whity/features/mcp`.
- English and Arabic (right to left), light and dark, with Whity's design tokens.

- **A local bridge for tools that can't sign in themselves.** Windsurf and Claude Desktop run `whity-agent mcp --connection <id>`, a stdio MCP server that relays to your workspace. Their config holds that command and nothing secret; the sign-in stays in the keychain (`src-tauri/src/bridge.rs`).

- **Chat with your workspace.** Runs Claude Code or Codex headless on your own account with them, connected through the bridge, with Whity's tools and nothing else: no files, no shell, none of your other MCP servers, hooks or plugins (`src-tauri/src/chat.rs`). Your words go over stdin, and tokens stay out of it. The tool keeps its own session files as it always does; Whity Agent doesn't save the conversation.

- **Act as one of your agents.** Agents you've granted appear under their workspace. Signing in as one adds a connection to `/mcp/agents/<slug>`, which the chat and the tools can use in place of you. The server enforces that agent's grant.

- **In the tray.** One copy runs at a time, closing the window keeps it running, and starting at login is your choice. A `whity-agent://connect?instance=<address>` link only fills in the address (`src-tauri/src/desktop.rs`).
- **Installers and signed updates.** See [install notes, in English and Arabic](docs/INSTALL.md). OS code signing is off for now; updates are always minisign-signed against the key in `src-tauri/updater.pub`. Releases are built as drafts by `.github/workflows/release.yml` and a person publishes them.

## The PHP runtime

Whity plugins are PHP. The app ships [FrankenPHP](https://frankenphp.dev) and
runs them on the device, in a child process Rust starts and supervises
(`crates/php-host`), serving the offline host in `php-host/` ([ADR 0001](docs/adr/0001-whity-desktop-foundations.md),
decision 2a). PHP runs under a sandbox ini the app writes at every start:
`open_basedir` covers the host's own code and its data directory only,
process and socket functions are disabled, and URL streams are off. The
window reaches plugin routes only through `php_request`, on loopback.

Before the first `npm run tauri dev` or `build`:

```bash
npm run php:setup
```

fetches the pinned release for this computer (Windows x64, Linux x64 or macOS
arm64) and checks it against the digest GitHub publishes for the asset. The
plugin SDK is copied beside the host by `npm run dev` and `npm run build`
(`scripts/copy-sdk.mjs`): from `WHITY_CORE_DIR`, a sibling `../whity-core`
checkout, or whity-core's `develop` on GitHub. Without the runtime the app
still works and reports the PHP host as not installed.

To boot the real runtime in a test: `WHITY_PHP_HOST_SMOKE=1 cargo test -p whity-php-host -- --ignored`.

## Hardware for plugins

Printers, and later scanners, card readers and serial devices, are Rust
crates behind one `Driver` trait in `crates/drivers`, each a Cargo feature.
Plugin PHP reaches them through a loopback bridge: at boot the host asks for
one handle per loaded plugin, bound to the capabilities that plugin declares
with `getDesktopCapabilities()` (for example `['printer.print']`), and the
bridge refuses a call the plugin never declared or one the device has Off.
Printing is Allowed by default; everything else is Off until the settings
screen lands ([ADR 0001](docs/adr/0001-whity-desktop-foundations.md), 3a and
3b). `GET /__whity/native-bridge` on the host shows what each plugin declared.

End to end, with the real runtime and a fixture plugin:
`WHITY_PHP_HOST_SMOKE=1 cargo test -p whity-drivers --test e2e -- --ignored`.

## Develop

You need Node 22, and Rust from `rust-toolchain.toml` (rustup installs it). On Linux, the Tauri system libraries and `libcups2-dev` for the printer driver (see `.github/workflows/ci.yml`).

```bash
npm install
npm run tauri dev
```

Checks, as CI runs them:

```bash
npm run typecheck && npm test
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

CI also checks the `changelog.d` fragments with whity-core's changelog tool (see
`changelog.d/README.md`).

## Design tokens

Tokens and fonts come from `@whity/tokens` and components from `@whity/ui`, both on npm. Import a component by its own path (`@whity/ui/button`), so the app bundles only what it uses. Use tokens only, never raw colours.

Screens follow whity-core's `docs/design`. whity-agent's own screens have no design cards yet; they are on the brand team's list (WHIT-821).

## Changes

Add a fragment to `changelog.d/` with every PR (see its README).

## Licence

MIT.
