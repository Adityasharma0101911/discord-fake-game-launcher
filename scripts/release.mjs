// Builds, signs and publishes a release from this machine.
// Usage: npm run release -- X.Y.Z [--dry-run]
// --dry-run builds and writes the upload files to dist/ without changing git or GitHub.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const REPO = 'Adityasharma0101911/discord-fake-game-launcher';
const PRODUCT = 'Endo’s Discord Game Spoofer';
const root = path.resolve(import.meta.dirname, '..');
const dist = path.join(root, 'dist');

const args = process.argv.slice(2);
const dryRun = args.includes('--dry-run');
const version = args.find((a) => !a.startsWith('--'));
const tag = `release_${version}`;
const key = process.env.TAURI_SIGNING_PRIVATE_KEY || path.join(os.homedir(), '.tauri', 'endos-discord-game-spoofer.key');

function fail(message) {
  console.error(message);
  process.exit(1);
}
const run = (cmd, cmdArgs, opts = {}) => execFileSync(cmd, cmdArgs, { cwd: root, stdio: 'inherit', ...opts });
const out = (cmd, cmdArgs) => execFileSync(cmd, cmdArgs, { cwd: root, encoding: 'utf8' }).trim();
const tryOut = (cmd, cmdArgs) => {
  try {
    return execFileSync(cmd, cmdArgs, { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
  } catch {
    return '';
  }
};

if (!/^\d+\.\d+\.\d+$/.test(version ?? '')) fail('Usage: npm run release -- X.Y.Z [--dry-run]');
if (!process.env.TAURI_SIGNING_PRIVATE_KEY && !fs.existsSync(key)) fail(`Signing key not found: ${key}`);
if (!dryRun) {
  if (out('git', ['status', '--porcelain'])) fail('Commit or stash your changes first.');
  if (out('git', ['tag', '--list', tag])) fail(`Tag ${tag} already exists.`);
  out('gh', ['auth', 'status']);
}

// The version is passed to the build directly; package.json is bumped only once the build has worked.
const config = { version, bundle: { createUpdaterArtifacts: true } };
run(process.execPath, [path.join(root, 'node_modules', '@tauri-apps', 'cli', 'tauri.js'), 'build', '--config', JSON.stringify(config)], {
  env: { ...process.env, TAURI_SIGNING_PRIVATE_KEY: key, TAURI_SIGNING_PRIVATE_KEY_PASSWORD: process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? '' }
});

// GitHub replaces characters like ’ and spaces in asset names, so upload under a plain name.
const built = path.join(root, 'src-tauri', 'target', 'release', 'bundle', 'nsis', `${PRODUCT}_${version}_x64-setup.exe`);
if (!fs.existsSync(`${built}.sig`)) fail(`Build finished but ${built}.sig is missing; the update would not be signed.`);
const installer = `EndosDiscordGameSpoofer_${version}_x64-setup.exe`;
fs.rmSync(dist, { recursive: true, force: true });
fs.mkdirSync(dist);
fs.copyFileSync(built, path.join(dist, installer));
fs.copyFileSync(`${built}.sig`, path.join(dist, `${installer}.sig`));

// Shown in the app's update dialog: commit subjects since the previous release.
const previous = tryOut('git', ['describe', '--tags', '--abbrev=0', '--match', 'release_*']);
const notes = previous ? out('git', ['log', '--format=- %s', `${previous}..HEAD`]) : '';
const platform = {
  signature: fs.readFileSync(`${built}.sig`, 'utf8').trim(),
  url: `https://github.com/${REPO}/releases/download/${tag}/${installer}`
};
const latest = {
  version,
  notes: notes || `${PRODUCT} ${version}`,
  pub_date: new Date().toISOString(),
  platforms: { 'windows-x86_64': platform, 'windows-x86_64-nsis': platform }
};
fs.writeFileSync(path.join(dist, 'latest.json'), `${JSON.stringify(latest, null, 2)}\n`);
const files = [installer, `${installer}.sig`, 'latest.json'].map((f) => path.join(dist, f));

if (dryRun) {
  console.log(`\nDry run: built ${version}; would commit, tag ${tag}, push and upload:\n${files.join('\n')}`);
  process.exit(0);
}

// What `npm version --no-git-tag-version` does, without spawning npm.cmd through a shell.
for (const file of ['package.json', 'package-lock.json']) {
  const json = JSON.parse(fs.readFileSync(path.join(root, file), 'utf8'));
  json.version = version;
  if (json.packages?.['']) json.packages[''].version = version;
  fs.writeFileSync(path.join(root, file), `${JSON.stringify(json, null, 2)}\n`);
}
run('git', ['commit', '-am', `Release ${version}`]);
run('git', ['tag', tag]);
run('git', ['push', 'origin', 'HEAD', tag]);
run('gh', ['release', 'create', tag, ...files, '--repo', REPO, '--title', `${PRODUCT} ${version}`, '--generate-notes']);
console.log(`\nReleased ${version}: https://github.com/${REPO}/releases/tag/${tag}`);
