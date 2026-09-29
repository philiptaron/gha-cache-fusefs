// Helpers shared by main.mjs and post.mjs. No dependencies: this action runs
// straight from its checkout.
import { spawnSync } from 'node:child_process';
import { appendFileSync } from 'node:fs';
import { EOL } from 'node:os';

export function input(name, fallback = '') {
  const v = process.env[`INPUT_${name.replace(/ /g, '_').toUpperCase()}`];
  return (v ?? '').trim() || fallback;
}

export function bool(name) {
  return input(name, 'false').toLowerCase() === 'true';
}

function append(file, name, value) {
  if (file) appendFileSync(file, `${name}=${value}${EOL}`);
}

export const setState = (name, value) => append(process.env.GITHUB_STATE, name, value);
export const setOutput = (name, value) => append(process.env.GITHUB_OUTPUT, name, value);
export const getState = name => process.env[`STATE_${name}`] ?? '';

export function run(cmd, args, options = {}) {
  const r = spawnSync(cmd, args, { stdio: 'inherit', ...options });
  if (r.error) throw new Error(`${cmd}: ${r.error.message}`);
  return r;
}

export function error(message) {
  console.log(`::error::${message}`);
}

export function warning(message) {
  console.log(`::warning::${message}`);
}
