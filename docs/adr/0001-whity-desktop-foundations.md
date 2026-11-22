# ADR 0001: Whity Desktop foundations

Status: accepted by the owner on 2026-11-06 ("Yes, all six"), written 2026-11-22.
Supersedes nothing; extends part B of whity-core's
`docs/design/AGENT-PERSONAS-AND-WHITY-AGENT.md`.

## Context

Whity's desktop companion exists in two halves that have never met.

**whity-agent** (this repository, 0.1.0) has the identity and agent half:
sign-in with Whity ID over OAuth 2.1 with PKCE and a loopback redirect,
refresh tokens in the OS keychain only, tokens that never reach the webview,
discovery and safe connection of local AI tools, a stdio MCP bridge, chat
through headless Claude Code or Codex, acting as one of the person's agents
through `/mcp/agents/<slug>`, a tray, single instance and a `whity-agent://`
link. It has no PHP runtime, no local database, no sync and no hardware.

**`templates/tauri-desktop`** in whity-core (0.2.0) has the edge half: a
FrankenPHP sidecar that runs unmodified plugin PHP against SQLite, plugins
reconciled from core's catalogue on every login, self-update served by core
and verified with minisign, a sync engine with row versions, optimistic
`409`s and a field-level conflict resolver, a 72-hour offline lock, a printer
crate and a loopback native bridge that PHP calls. It signs in with email and
password, its CSP is off, the device role defaults to admin with every plugin
permission, the bridge has one secret shared by the whole PHP host, and
`\Whity\app()` hands any plugin the database. Its Rust is not under CI and
macOS is untried.

Two constraints from elsewhere in Whity shape this document:

- **Agents propose, people approve.** Every agent permission is Off, Ask
  first or Allowed, decided by the host, never by the plugin or the agent
  (whity-core `ToolSurface::readsOnly()`, GHSA-9jfp-rcc5-wwm2).
- **The plugin boundary was too trusting** (the 0.2.27 security sweep). The
  edge must not repeat it: plugins on a person's own computer can reach
  hardware and files the server never could.

## Decisions

### 1. whity-agent is the base; the repository becomes `whity-desktop`

The template's PHP host, sync engine and plugin reconciler move into this
repository as crates in a Cargo workspace (`crates/php-host`, `crates/sync`,
`crates/plugins`, `crates/drivers`). The template in whity-core becomes a
pointer to this repository once the move lands; its `php-host/` PHP code
moves here with it, under the same byte-identical SDK copy rule.

Product name "Whity Desktop", identifier `dev.whity.desktop`. The
`whity-agent` binary name and the `whity-agent mcp --connection <id>` bridge
command stay as they are, so the tool configs already written keep working.

Why this way round: whity-agent already has the security posture the app
must end with (Whity ID, keychain-only tokens, a strict CSP, `core:default`
capabilities only, Rust under CI). Porting engines into a hardened shell is
safer than hardening a shell around engines. Two separate apps were rejected:
one companion, one tray icon, one sign-in.

### 2a. The PHP runtime is a FrankenPHP sidecar

The app ships FrankenPHP as a sidecar process, PHP pinned at 8.4.27 or later
(never below; CVE floor). The supervisor (`sidecar.rs`, Windows Job Object)
stays. In-process libphp was rejected: a PHP crash would take the app down,
and a separate process is the only boundary an operating system can lock
down (no network, a read-only code directory, a writable data directory and
nothing else). A static `php-cli` per request was rejected as too slow for
screens.

### 2b. Updates are signed by Whity and delivered through core

One Whity release key, held by a person, never in CI's control beyond the
write-only secret, as `docs/wiki/Desktop-App-Updates.md` already prescribes.
Its public half is built into the app. Every update of the app (which
includes the PHP runtime and the PHP host) is minisign-verified against it,
whatever the delivery path.

Delivery: when signed in, the app asks the workspace's core instance
(`GET /api/desktop-app-updates/latest`), so an operator decides which
version their workspace runs and when. Signed out, or when the instance has
no release registered, the app falls back to Whity's own feed
(`latest.json` on GitHub Releases). Both answers must verify against the
same key; an instance cannot ship its own binary.

Plugin packages follow the store decision of 2026-11-11: packages are
content-addressed by SHA-256 and signed with the store's Ed25519 key in
Whity ID. The device verifies hash and signature before extraction. The
operator-only "allow unsigned" override that core has does not exist on
desktop (decision 3a).

### 3a. Edge plugins run under a declared-capability sandbox

Only signed plugins run on the edge. Each declares what it needs in its
manifest, for example `desktop.capabilities: ["printer.print",
"serial.open"]`. At load the host issues each plugin an opaque handle; the
native bridge accepts a call only with a valid handle and only for a
capability that plugin declared and the person has set to Allowed (or Ask
first, which raises an `ActionProposal` in the app before the call runs).
Settings are per device and per plugin, Off by default for anything beyond
`printer.print`.

The PHP side is hardened with `open_basedir`, `disable_functions` for
process and network functions, `allow_url_fopen=0`, and a host container
that registers only the services the plugin declared. `\Whity\app()` with
no declaration returns nothing.

Honest limit: plugins sharing one PHP process cannot be fully isolated from
each other. Signing is the gate that makes the shared process acceptable. A
process per trust level (store plugins apart from the workspace's own) is
the planned next step, not this one.

### 3b. Hardware drivers are Rust crates behind one trait

```rust
pub trait Driver: Send + Sync {
    fn id(&self) -> &'static str;                 // "printer", "serial", "card-reader"
    fn capabilities(&self) -> &'static [&'static str];
    fn call(&self, op: &str, args: serde_json::Value) -> Result<serde_json::Value, DriverError>;
}
```

Drivers are compiled in behind Cargo features (`driver-printer`,
`driver-serial`, `driver-scanner`, `driver-card-reader`), registered at
start and reached only through the native bridge above. Dynamic libraries
were rejected (unsafe, unsignable). Out-of-process driver sidecars speaking
JSON-RPC over stdio are the later path for third parties.

### 4. Offline sync keeps the template's engine; conflicts are resolved by people

Per-row versions, idempotent create on `client_uuid`, optimistic `baseVersion`
with `409`, tombstones and a cursor-based changes feed, as the template and
whity-core's `SyncableResource` / `SyncController` already share. Disjoint
field changes merge on their own; a true conflict is parked and resolved
field by field in the shared `ConflictResolver`. Last-write-wins and CRDTs
were rejected: business records need a person to decide.

Rules that follow from "people approve":

- An agent working offline produces proposals, not writes. They sync as
  proposals and are approved online.
- Final document numbers are issued by the server only. Offline, a document
  is a draft with a provisional reference.
- Only resources that declare `SyncableResource` sync. Core resources without
  a changes feed stay remote-only until core grows one.
- The PHP host's SQLite is the single owner of plugin data on the device;
  the Rust relay (`sync/bridge.rs`) is switched on per resource only once
  the server end exists, with a per-leg path.

### 5. Sign-in is PKCE over loopback, then device enrolment

Sign-in stays as whity-agent does it: OAuth 2.1 with PKCE S256, loopback
redirect on `127.0.0.1` with a one-time port, the published client document
at Whity ID. A deep link redirect was rejected: Whity ID refuses private-use
schemes that are not reverse-DNS, and a loopback redirect cannot be claimed
by another app. `whity-agent://` links only open the app and fill in an
address.

Whity ID access tokens are bound to one resource (`<instance>/mcp` or
`/mcp/agents/<slug>`). For core's REST API (sync, plugin catalogue, updates)
the app enrols the device after sign-in (`POST /api/v1/devices`), using the
Whity ID session, and keeps the device credential in the keychain beside
the refresh token. The offline lock (`auth.desktop_login_max_hours`) applies
to that credential. Whether core's REST accepts Whity ID tokens directly is
open; if it does, enrolment becomes an optimisation rather than a need.

### 6. Windows first; releases are drafts a person publishes

Targets in order: Windows x64 (verified), macOS arm64 (needs a Mac and an
Apple Developer ID), Linux x64 (AppImage and deb; needs `libdbus-1`).
Channels: `stable` and `beta`, chosen per device, carried as a query
parameter to the update endpoint. Releases are built as drafts by CI and
published by a person, keeping the owner's release rule. OS code signing
stays off until the owner buys a Windows certificate and an Apple Developer
ID (open); update signing is never off.

## Consequences

- One repository, one product, one sign-in. The whity-core template is
  retired in favour of a pointer.
- Plugin authors declare desktop capabilities in the manifest; the SDK
  grows `desktop.capabilities` and a `Desktop` facade in a minor version
  (number from the orchestrator).
- Core grows nothing new for the first PRs; the device, update and plugin
  endpoints exist. Sync endpoints for core resources, per-workspace plugin
  releases and the channel parameter are later core PRs.
- A person on a device can see and change every plugin's hardware
  capabilities, in the same Off / Ask first / Allowed words as agents.

## Build plan: small PRs, in order

1. Rename to whity-desktop; Cargo workspace; this ADR. (This PR, minus the rename.)
2. `crates/php-host`: sidecar supervisor, proxy, FrankenPHP setup scripts, PHP host code, hardened `php.ini`.
3. `crates/drivers`: `Driver` trait, printer driver, native bridge with per-plugin handles.
4. Capability manifest, handles and the Off / Ask first / Allowed screen.
5. `crates/plugins`: signed package verify, reconcile from the store.
6. Device enrolment after Whity ID sign-in; offline lock.
7. `crates/sync`: engine, scheduler, conflict resolver screen.
8. Core-served updates with fallback feed; channels.
9. Rust under CI on all three targets; macOS and Linux builds.
