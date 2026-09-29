// Mounts the cache: finds or builds the binary, then starts the daemon, which
// detaches once the filesystem is live. post.mjs unmounts it.
import { chmodSync, existsSync, mkdirSync, writeFileSync } from 'node:fs';
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
async function download(dest) {
  const arch = { x64: 'x86_64', arm64: 'aarch64' }[process.arch];
  const ref = process.env.GITHUB_ACTION_REF;
  const repo = process.env.GITHUB_ACTION_REPOSITORY || 'philiptaron/gha-cache-fusefs';
  if (!arch || !ref) return false;
  for (const tag of [ref, `${ref}-latest`]) {
    const url = `https://github.com/${repo}/releases/download/${tag}/gha-cache-fusefs-${arch}-linux`;
    const resp = await fetch(url);
    if (!resp.ok) continue;
    console.log(`Downloaded ${url}`);
    writeFileSync(dest, Buffer.from(await resp.arrayBuffer()));
    chmodSync(dest, 0o755);
    return true;
  }
  return false;
}

function build(cmd, args, out) {
  const r = spawnSync(cmd, args, { stdio: ['ignore', 'pipe', 'inherit'], encoding: 'utf8', cwd: repoRoot });
  if (r.status === 0) return out(r.stdout.trim());
  error(`${cmd} ${args.join(' ')} failed (exit ${r.status})`);
  return undefined;
}

async function binary(temp) {
  const given = input('binary');
  if (given) return resolve(given);
  const dest = join(temp, 'gha-cache-fusefs');
  if (await download(dest)) return dest;
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
    const target = join(temp, 'gha-cache-fusefs-target');
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
  const temp = process.env.RUNNER_TEMP || '/tmp';
  const bin = await binary(temp);
  if (!existsSync(bin)) throw new Error(`${bin} does not exist`);
  const hash = createHash('sha256').update(path).digest('hex').slice(0, 12);
  const stateDir = join(temp, 'gha-cache-fusefs', `mount-${hash}`);
  ensureDir(path);
  mkdirSync(stateDir, { recursive: true });

  const args = [
    'mount', path, '--daemon',
    '--state-dir', stateDir,
    '--prefix', input('prefix', 'fusefs/'),
    '--settle', input('settle', '1s'),
    '--cache-size-mb', input('cache-size-mb', '8192'),
    '--log', input('log', 'info'),
  ];
  if (bool('read-only')) args.push('--read-only');
  if (bool('gc')) args.push('--gc');
  // Save state first: even a failed mount should be looked at by post.
  setState('path', path);
  setState('binary', bin);
  setState('state-dir', stateDir);
  setOutput('state-dir', stateDir);
  const r = run(bin, args, { env: { ...process.env, GITHUB_TOKEN: input('token') } });
  if (r.status !== 0) throw new Error(`mounting failed (exit ${r.status}); see ${stateDir}/daemon.log`);
}

main().catch(e => {
  error(e.message);
  process.exitCode = 1;
});
