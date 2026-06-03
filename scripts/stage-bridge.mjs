// Stage the companion bridge so the IDE's `ensure_bridge_running`
// (ADR 0024) can find and auto-start it, for both build paths:
//
//   - bundled build (`bun run build`): the bridge + PWA must live in
//     `src-tauri/resources/bridge/` *before* `tauri build` runs, so
//     the bundler copies them into the app's resource dir
//     (`<resource>/bridge/...`). That's the `prepare` step.
//   - --no-bundle (`bun run build:bin`): the exe runs straight from
//     `target/<profile>/`, so after the build we also drop the bridge
//     + PWA next to it (`target/<profile>/{moon-bridge, companion/}`).
//     That's the `exe-adjacent` step.
//
// `tauri build` only compiles the desktop binary; `moon-bridge` is a
// separate workspace member and the PWA a separate Vite app, so this
// script owns building + placing them. The IDE tries the resource dir
// first, then exe-adjacent (see `ensure_bridge_running`).
//
// Usage:
//   node scripts/stage-bridge.mjs prepare        # before tauri build
//   node scripts/stage-bridge.mjs exe-adjacent   # after tauri build --no-bundle
// Append `--debug` for a debug-profile build.

import { execFileSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, renameSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = dirname(dirname(fileURLToPath(import.meta.url)));
const mode = process.argv[2];
const profile = process.argv.includes('--debug') ? 'debug' : 'release';
const bridgeName = process.platform === 'win32' ? 'moon-bridge.exe' : 'moon-bridge';

if (mode !== 'prepare' && mode !== 'exe-adjacent') {
	console.error('usage: stage-bridge.mjs <prepare|exe-adjacent> [--debug]');
	process.exit(2);
}

function run(cmd, args) {
	console.log(`> ${cmd} ${args.join(' ')}`);
	execFileSync(cmd, args, { cwd: repoRoot, stdio: 'inherit' });
}

function builtBridgePath() {
	const bin = join(repoRoot, 'target', profile, bridgeName);
	if (!existsSync(bin)) {
		throw new Error(`expected ${bin} after cargo build, but it is missing`);
	}
	return bin;
}

function builtDist() {
	const dist = join(repoRoot, 'companion', 'dist');
	if (!existsSync(dist)) {
		throw new Error(`companion/dist not found — run \`bun run build:companion\` first (looked in ${dist})`);
	}
	return dist;
}

/**
 * Copy the bridge binary + companion PWA into `destDir/`. Replaces
 * only the two artifacts this script owns — it does not nuke the
 * directory, so a tracked `.gitkeep` (kept in the resource dir so
 * tauri-build's path validation passes on a fresh checkout) survives.
 */
function placeInto(destDir) {
	mkdirSync(destDir, { recursive: true });

	const srcBin = builtBridgePath();
	const destBin = join(destDir, bridgeName);
	// In exe-adjacent mode destDir *is* target/<profile>, so the built
	// binary already sits at destBin — nothing to copy.
	if (resolve(srcBin) !== resolve(destBin)) {
		// Write to a temp name then atomic-rename over destBin.
		// rename(2) swaps the directory entry without touching a
		// running process's open inode, so this never hits
		// `ETXTBSY` ("Text file busy") when a previously-staged
		// bridge is still running from this path.
		const tmpBin = `${destBin}.new`;
		rmSync(tmpBin, { force: true });
		cpSync(srcBin, tmpBin);
		renameSync(tmpBin, destBin);
	}

	const destWeb = join(destDir, 'companion');
	rmSync(destWeb, { recursive: true, force: true });
	cpSync(builtDist(), destWeb, { recursive: true });
	console.log(`Staged bridge + companion into ${destDir}`);
}

// Both modes need the binary built into target/<profile>/.
run('cargo', ['build', '-p', 'moon-bridge', ...(profile === 'release' ? ['--release'] : [])]);

if (mode === 'prepare') {
	// Populate the tauri resource source dir so `tauri build` bundles
	// it (see tauri.conf.json > bundle > resources).
	placeInto(join(repoRoot, 'src-tauri', 'resources', 'bridge'));
} else {
	// Drop next to the built exe for the --no-bundle path. `placeInto`
	// is surgical (only its two artifacts), so it's safe to write
	// straight into target/<profile> alongside moon-desktop.
	placeInto(join(repoRoot, 'target', profile));
}
