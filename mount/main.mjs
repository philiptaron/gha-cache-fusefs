// Mounts the cache: finds or builds the binary, then starts the daemon, which
// detaches once the filesystem is live. post.mjs unmounts it.
//
// Everything this action keeps lives under $RUNNER_TEMP/gha-cache-fusefs:
// bin/<release>/ (downloads), target/ (cargo builds), mount-<hash>/ (state).
import { chmodSync, existsSync, mkdirSync, renameSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';

import { bool, error, input, run, setOutput, setState } from './common.mjs';

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function onPath(cmd) {
  return spawnSync('sh', ['-c', `command -v ${cmd}`], { stdio: 'ignore' }).status === 0;
}

// Releases are published by the fusefs-release workflow: one per tag, and a
// rolling `<branch>-latest` prerelease for each push to a release branch.
// The `release` input picks one; by default the action's own ref does.
async function download(home) {
  const arch = { x64: 'x86_64', arm64: 'aarch64' }[process.arch];
  const ref = process.env.GITHUB_ACTION_REF;
  const repo = process.env.GITHUB_ACTION_REPOSITORY || 'philiptaron/gha-cache-fusefs';
  const pinned = input('release');
  const tags = pinned ? [pinned] : ref ? [ref, `${ref}-latest`] : [];
  if (!arch) return undefined;
  for (const tag of tags) {
    const dest = join(home, 'bin', tag, 'gha-cache-fusefs');
    // A second mount in the same job reuses the first one's download.
    if (existsSync(dest)) return dest;
    const url = `https://github.com/${repo}/releases/download/${tag}/gha-cache-fusefs-${arch}-linux`;
    const resp = await fetch(url);
    if (!resp.ok) continue;
    console.log(`Downloaded ${url}`);
    mkdirSync(dirname(dest), { recursive: true });
    // Rename into place, so a running daemon's executable is never rewritten.
    writeFileSync(`${dest}.tmp`, Buffer.from(await resp.arrayBuffer()));
    chmodSync(`${dest}.tmp`, 0o755);
    renameSync(`${dest}.tmp`, dest);
    return dest;
  }
  if (pinned) throw new Error(`release ${pinned} has no gha-cache-fusefs-${arch}-linux`);
  return undefined;
}

function build(cmd, args, out) {
  const r = spawnSync(cmd, args, { stdio: ['ignore', 'pipe', 'inherit'], encoding: 'utf8', cwd: repoRoot });
  if (r.status === 0) return out(r.stdout.trim());
  error(`${cmd} ${args.join(' ')} failed (exit ${r.status})`);
  return undefined;
}

async function binary(home) {
  const given = input('binary');
  if (given) return resolve(given);
  const downloaded = await download(home);
  if (downloaded) return downloaded;
  if (onPath('nix')) {
    console.log('Building gha-cache-fusefs with Nix');
    const nixArgs = ['--extra-experimental-features', 'nix-command flakes', 'build', `path:${repoRoot}#static`, '--no-link', '--print-out-paths'];
    const bin = build('nix', nixArgs, out => join(out, 'bin', 'gha-cache-fusefs'));
    if (bin) return bin;
  }
  if (onPath('cargo')) {
    // Hosted runners come with Rust; this takes a couple of minutes.
    console.log('Building gha-cache-fusefs with cargo');
    const manifest = join(repoRoot, 'fusefs', 'Cargo.toml');
    const target = join(home, 'target');
    const bin = build('cargo', ['build', '--release', '--locked', '--manifest-path', manifest, '--target-dir', target], () =>
      join(target, 'release', 'gha-cache-fusefs')
    );
    if (bin) return bin;
  }
  throw new Error('No gha-cache-fusefs binary: pass `binary`, or make Nix or cargo available before this step');
}

function ensureDir(path) {
  try {
    mkdirSync(path, { recursive: true });
  } catch (e) {
    if (e.code !== 'EACCES' && e.code !== 'EPERM') throw e;
    // For mountpoints like /mnt/cache: the runner user has passwordless sudo.
    run('sudo', ['mkdir', '-p', path]);
    run('sudo', ['chown', `${process.getuid()}:${process.getgid()}`, path]);
  }
}

async function main() {
  if (process.platform !== 'linux') throw new Error('mounting the cache needs Linux');
  const path = resolve(input('path'));
  const home = join(process.env.RUNNER_TEMP || '/tmp', 'gha-cache-fusefs');
  const bin = await binary(home);
  if (!existsSync(bin)) throw new Error(`${bin} does not exist`);
  const hash = createHash('sha256').update(path).digest('hex').slice(0, 12);
  const stateDir = join(home, `mount-${hash}`);
  ensureDir(path);
  mkdirSync(stateDir, { recursive: true });

  const args = [
    'mount', path, '--daemon',
    '--state-dir', stateDir,
    '--settle', input('settle', '1s'),
    '--cache-size-mb', input('cache-size-mb', '8192'),
    '--log', input('log', 'info'),
  ];
  if (bool('read-only')) args.push('--read-only');
  // Save state first: even a failed mount should be looked at by post.
  setState('path', path);
  setState('binary', bin);
  setState('state-dir', stateDir);
  setOutput('state-dir', stateDir);
  // The volume, root, and fsync mode go in the environment rather than as
  // flags: a binary from before they existed, such as an older release,
  // ignores them instead of refusing to start.
  const env = {
    ...process.env,
    GITHUB_TOKEN: input('token'),
    GHA_CACHE_FUSEFS_VOLUME: input('volume', 'default'),
    GHA_CACHE_FUSEFS_ROOT: input('root', ''),
    GHA_CACHE_FUSEFS_FSYNC: input('fsync', 'local'),
  };
  const r = run(bin, args, { env });
  if (r.status !== 0) throw new Error(`mounting failed (exit ${r.status}); see ${stateDir}/daemon.log`);
}

main().catch(e => {
  error(e.message);
  process.exitCode = 1;
});
