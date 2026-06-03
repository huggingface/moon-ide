// Stage the companion bridge next to the IDE binary so the IDE's
// `ensure_bridge_running` (ADR 0024) can find and auto-start it.
//
// `tauri build` only compiles the desktop binary; `moon-bridge` is a
// separate workspace member and the companion PWA is a separate Vite
// app. This script builds both and copies them into the same
// directory as the IDE exe (`target/<profile>/`), which is what
// `current_exe().parent()` resolves to for a `--no-bundle` build:
//
//   target/<profile>/moon-bridge        (binary)
//   target/<profile>/companion/         (built PWA, = companion/dist)
//
// Run after the IDE + companion builds (see package.json `build:bin`
// / `build`). Bundled installers need these as tauri `resources` /
// sidecar instead — tracked as a follow-up; this covers the
// `--no-bundle` path the team uses day to day.

import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const profile = process.argv.includes('--debug') ? 'debug' : 'release';
const targetDir = join(repoRoot, 'target', profile);
const bridgeName = process.platform === 'win32' ? 'moon-bridge.exe' : 'moon-bridge';

function run(cmd, args) {
	console.log(`> ${cmd} ${args.join(' ')}`);
	execFileSync(cmd, args, { cwd: repoRoot, stdio: 'inherit' });
}

// 1. Build the moon-bridge binary into the same target profile dir.
run('cargo', ['build', '-p', 'moon-bridge', ...(profile === 'release' ? ['--release'] : [])]);

// 2. Stage the built PWA next to the binary as `companion/`.
const dist = join(repoRoot, 'companion', 'dist');
if (!existsSync(dist)) {
	throw new Error(`companion/dist not found — run \`bun run build:companion\` first (looked in ${dist})`);
}
const stagedWeb = join(targetDir, 'companion');
rmSync(stagedWeb, { recursive: true, force: true });
mkdirSync(targetDir, { recursive: true });
cpSync(dist, stagedWeb, { recursive: true });

// 3. Sanity-check the binary landed where we expect.
const bridgeBin = join(targetDir, bridgeName);
if (!existsSync(bridgeBin)) {
	throw new Error(`expected ${bridgeBin} after cargo build, but it is missing`);
}

console.log(`Staged companion bridge: ${bridgeBin} + ${stagedWeb}`);
