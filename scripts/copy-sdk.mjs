// Copies whity-core's plugin SDK (sdk/src) into php-host/sdk/src, the contract
// types a plugin on the device implements. A device has no network and no
// Composer, so the SDK ships beside the host; the copy is made before every
// dev and build and is not committed.
//
// Source, in order: WHITY_CORE_DIR, a sibling ../whity-core checkout, or the
// public whity-core repository on GitHub at WHITY_CORE_REF (default: develop).
import { cpSync, existsSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { execFileSync } from 'node:child_process'
import { writeFileSync } from 'node:fs'

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const target = join(repoRoot, 'php-host', 'sdk', 'src')

const candidates = [process.env.WHITY_CORE_DIR, resolve(repoRoot, '..', 'whity-core')].filter(Boolean)
let source = candidates.map((dir) => join(dir, 'sdk', 'src')).find((dir) => existsSync(join(dir, 'Sdk.php')))

let scratch = null
if (!source) {
  const ref = process.env.WHITY_CORE_REF ?? 'develop'
  const url = `https://codeload.github.com/whity-org/whity-core/tar.gz/${ref}`
  console.log(`No whity-core checkout found; fetching sdk/src from ${url}`)
  const response = await fetch(url, { redirect: 'follow' })
  if (!response.ok) {
    console.error(`Fetching whity-core failed: ${response.status} ${response.statusText}`)
    process.exit(1)
  }
  scratch = mkdtempSync(join(tmpdir(), 'whity-core-sdk-'))
  writeFileSync(join(scratch, 'core.tar.gz'), Buffer.from(await response.arrayBuffer()))
  // The whole tree is unpacked (pattern flags differ between GNU tar and
  // bsdtar), from inside the scratch directory with relative names: a `C:` in
  // an argument makes GNU tar look for a host called C.
  execFileSync('tar', ['-xzf', 'core.tar.gz', '--strip-components=1'], { cwd: scratch, stdio: 'inherit' })
  source = join(scratch, 'sdk', 'src')
}

rmSync(target, { recursive: true, force: true })
cpSync(source, target, { recursive: true })
if (scratch) rmSync(scratch, { recursive: true, force: true })
console.log(`Copied the plugin SDK from ${source} into ${target}`)
