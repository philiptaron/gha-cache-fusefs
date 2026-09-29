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

async function download(dest) {
  const arch = { x64: 'x86_64', arm64: 'aarch64' }[process.arch];
  const ref = process.env.GITHUB_ACTION_REF;
  if (!arch || !ref) return false;
  const url = `https://github.com/philiptaron/gha-cache-fusefs/releases/download/${ref}/gha-cache-fusefs-${arch}-linux`;
  console.log(`Downloading ${url}`);
  const resp = await fetch(url);
  if (!resp.ok) {
    console.log(`  ${resp.status} ${resp.statusText}`);
    return false;
  }
  writeFileSync(dest, Buffer.from(await resp.arrayBuffer()));
  chmodSync(dest, 0o755);
  return true;
}

async function binary(temp) {
  const given = input('binary');
  if (given) return resolve(given);
  if (onPath('nix')) {
    console.log('Building gha-cache-fusefs with Nix');
    const r = spawnSync(
      'nix',
      ['--extra-experimental-features', 'nix-command flakes', 'build', `path:${repoRoot}#static`, '--no-link', '--print-out-paths'],
      { stdio: ['ignore', 'pipe', 'inherit'], encoding: 'utf8' }
    );
    if (r.status === 0) return join(r.stdout.trim(), 'bin', 'gha-cache-fusefs');
    error('nix build failed; trying a release download');
  }
  const dest = join(temp, 'gha-cache-fusefs');
  if (await download(dest)) return dest;
  throw new Error(
    'No gha-cache-fusefs binary: pass `binary`, install Nix before this step, or use a released ref of this action'
  );
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
