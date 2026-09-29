// Unmounts the cache and waits for pending uploads, then reports what was
// saved in the job summary.
import { appendFileSync, existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

import { bool, error, getState, run, warning } from './common.mjs';

function human(bytes) {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  while (bytes >= 1024 && i < units.length - 1) {
    bytes /= 1024;
    i++;
  }
  return `${bytes.toFixed(i ? 1 : 0)} ${units[i]}`;
}

function report(path, summary) {
  const lines = [
    `### Cache mount \`${path}\``,
    '',
    '| uploaded files | uploaded | whiteouts | directory markers | downloaded | failures |',
    '|---:|---:|---:|---:|---:|---:|',
    `| ${summary.uploaded_files} | ${human(summary.uploaded_bytes)} | ${summary.whiteouts} | ${summary.dir_markers} | ${human(summary.downloaded_bytes)} | ${summary.failures.length} |`,
    '',
  ];
  const q = summary.requests;
  if (q) lines.push(`Requests: ${q.cache_service} to the cache service, ${q.blob} to blob storage, ${q.rest} to the REST API.`, '');
  if (summary.rate_limited) {
    const paused = Math.round((summary.rate_limit_pause_ms ?? 0) / 1000);
    lines.push(
      `:hourglass: Rate limited ${summary.rate_limited} times, which held up uploads (or listings) for ${paused} s. ` +
        'Every file, empty directory, and deletion is a new cache entry, and the service allows about 200 per ~45 s ' +
        'per repository; packing small files (tar, squashfs, zip) avoids the wait.',
      ''
    );
  }
  for (const f of summary.failures) lines.push(`- :x: \`${f.key}\`: ${f.error}`);
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, lines.join('\n') + '\n');
}

function main() {
  const path = getState('path');
  const bin = getState('binary');
  const stateDir = getState('state-dir');
  if (!path || !bin) {
    console.log('Nothing was mounted.');
    return;
  }
  console.log(`Unmounting ${path} and waiting for uploads`);
  const r = run(bin, ['unmount', path, '--state-dir', stateDir], {
    stdio: ['ignore', 'pipe', 'inherit'],
    encoding: 'utf8',
  });
  const log = join(stateDir, 'daemon.log');
  if (existsSync(log)) {
    console.log('::group::Daemon log');
    console.log(readFileSync(log, 'utf8'));
    console.log('::endgroup::');
  }
  let summary;
  try {
    summary = JSON.parse(r.stdout);
  } catch {
    summary = undefined;
  }
  if (summary) {
    console.log(JSON.stringify(summary, null, 2));
    report(path, summary);
  }
  if (r.status !== 0) {
    const message = summary
      ? `${summary.failures.length} change(s) to ${path} could not be saved to the cache`
      : `unmounting ${path} failed (exit ${r.status})`;
    if (bool('fail-on-error')) {
      error(message);
      process.exitCode = 1;
    } else {
      warning(message);
    }
  }
}

main();
