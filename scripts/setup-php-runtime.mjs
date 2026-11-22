// Fetches the pinned FrankenPHP release for this computer and stages it, with
// php-host/php.ini, under src-tauri/resources/frankenphp/, where
// tauri.conf.json bundles it and crates/php-host spawns it from. Run once on a
// fresh clone before `npm run tauri dev` or `npm run tauri build`:
//
//   npm run php:setup
//
// Pinned, never "latest": a build must not pick up a new runtime by itself.
// Each asset's SHA-256 is the digest GitHub publishes for the release asset,
// checked here before anything is unpacked. GitHub release assets can be
// re-uploaded under the same tag, and a changed digest refuses the download.
//
// PHP floor: never below 8.4.27 (a CVE). This release ships PHP 8.6.
import { createHash } from 'node:crypto'
import { chmodSync, cpSync, existsSync, mkdirSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { execFileSync } from 'node:child_process'

const VERSION = '1.14.0'
const ASSETS = {
  'win32-x64': {
    name: 'frankenphp-windows-x86_64.zip',
    sha256: '4cc1a9722e7d10a8722b23528faa8654c3362739b7b8c2997616630d1a939bdc',
  },
  'linux-x64': {
    name: 'frankenphp-linux-x86_64',
    sha256: '8fa49e1a389788bac2efc61a7ca70be134c05b115c0ad1eab5c6814d8fa6befb',
  },
  'darwin-arm64': {
    name: 'frankenphp-mac-arm64',
    sha256: '41b6cadfcce6267f3645bf0db8f1c74d9326d183b787b121400dc2624648426c',
  },
}

// On Windows, PHP extensions are DLLs loaded only when php.ini names them;
// everything else in the release is tooling the app never runs.
const KEEP_EXTENSIONS = new Set(['php_pdo_sqlite.dll', 'php_sqlite3.dll', 'php_mbstring.dll'])
const DROP_FROM_WINDOWS = [
  'dev', 'extras', 'lib', 'deplister.exe', 'php-win.exe', 'phpdbg.exe', 'php8phpdbg.dll',
  'php8embed.lib', 'phar.phar.bat', 'pharcommand.phar', 'php.ini-development',
  'php.ini-production', 'README.md', 'license.txt', 'news.txt', 'readme-redist-bins.txt', 'snapshot.txt',
]

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const dest = join(repoRoot, 'src-tauri', 'resources', 'frankenphp')
const key = `${process.platform}-${process.arch}`
const asset = ASSETS[key]
if (!asset) {
  console.error(`No FrankenPHP asset is pinned for ${key}. Pinned: ${Object.keys(ASSETS).join(', ')}.`)
  process.exit(1)
}

const url = `https://github.com/php/frankenphp/releases/download/v${VERSION}/${asset.name}`
console.log(`Downloading FrankenPHP v${VERSION} (${asset.name})...`)
const response = await fetch(url, { redirect: 'follow' })
if (!response.ok) {
  console.error(`Download failed: ${response.status} ${response.statusText}`)
  process.exit(1)
}
const bytes = Buffer.from(await response.arrayBuffer())
const actual = createHash('sha256').update(bytes).digest('hex')
if (actual !== asset.sha256) {
  console.error(`Checksum mismatch for ${asset.name}: expected ${asset.sha256}, got ${actual}. Refusing to stage it.`)
  process.exit(1)
}
console.log('Checksum verified.')

rmSync(dest, { recursive: true, force: true })
mkdirSync(dest, { recursive: true })

if (asset.name.endsWith('.zip')) {
  // Only the Windows asset is a zip. Expand-Archive is always there on
  // Windows; the `tar` on PATH may be Git's GNU tar, which reads no zip.
  const zipName = 'frankenphp.zip'
  writeFileSync(join(dest, zipName), bytes)
  execFileSync(
    'powershell',
    ['-NoProfile', '-Command', `Expand-Archive -LiteralPath '${zipName}' -DestinationPath . -Force`],
    { cwd: dest, stdio: 'inherit' },
  )
  rmSync(join(dest, zipName), { force: true })
  for (const entry of readdirSync(join(dest, 'ext'))) {
    if (entry.startsWith('php_') && entry.endsWith('.dll') && !KEEP_EXTENSIONS.has(entry)) {
      rmSync(join(dest, 'ext', entry), { force: true })
    }
  }
  for (const entry of DROP_FROM_WINDOWS) {
    rmSync(join(dest, entry), { recursive: true, force: true })
  }
} else {
  const binary = join(dest, 'frankenphp')
  writeFileSync(binary, bytes)
  chmodSync(binary, 0o755)
}

// The runtime's own php.ini sits beside the binary; the sandbox ini the app
// writes per device is added through PHP_INI_SCAN_DIR at run time.
cpSync(join(repoRoot, 'php-host', 'php.ini'), join(dest, 'php.ini'))
if (!existsSync(join(dest, 'php.ini'))) {
  console.error('php.ini was not staged')
  process.exit(1)
}
const staged = readdirSync(dest).length
console.log(`FrankenPHP v${VERSION} staged at ${dest} (${staged} entries). PHP: ${readFileSync(join(dest, 'php.ini'), 'utf8').split('\n')[0]}`)
