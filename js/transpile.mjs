/**
 * Transpile the component to JS, wiring every WASI import to our own host
 * (`src/host.js`) instead of `@bytecodealliance/preview2-shim`.
 *
 * A script rather than a line in `package.json` because there are twenty
 * mappings and one WASI version to keep them all agreeing on: an interface left
 * unmapped silently falls back to the shim, and the two hosts do not share a
 * filesystem — the component would then create its data directory in one and
 * look for it in the other.
 */
import { execFileSync } from 'node:child_process';

const WASI = '0.2.9';

/** Interface (without the version) → the export in `src/host.js` that implements it. */
const HOST = {
  'wasi:io/poll': 'poll',
  'wasi:io/streams': 'streams',
  'wasi:io/error': 'error',
  'wasi:cli/stdout': 'stdout',
  'wasi:cli/stderr': 'stderr',
  'wasi:cli/stdin': 'stdin',
  'wasi:cli/environment': 'environment',
  'wasi:cli/exit': 'exit',
  'wasi:cli/terminal-input': 'terminalInput',
  'wasi:cli/terminal-output': 'terminalOutput',
  'wasi:cli/terminal-stdin': 'terminalStdin',
  'wasi:cli/terminal-stdout': 'terminalStdout',
  'wasi:cli/terminal-stderr': 'terminalStderr',
  'wasi:clocks/monotonic-clock': 'monotonicClock',
  'wasi:clocks/wall-clock': 'wallClock',
  'wasi:random/random': 'random',
  'wasi:random/insecure-seed': 'insecureSeed',
  'wasi:filesystem/types': 'types',
  'wasi:filesystem/preopens': 'preopens',
};

const component =
  process.argv[2] ?? '../target/wasm32-wasip2/wasm-release/crabgresql_wasm.wasm';

const args = [
  'jco',
  'transpile',
  component,
  '-o',
  'dist',
  '--name',
  'crabgresql',
];
for (const [wasi, host] of Object.entries(HOST)) {
  // The path is relative to the *output* directory, which is where the
  // generated module imports it from.
  args.push('--map', `${wasi}@${WASI}=../src/host.js#${host}`);
}

execFileSync('npx', args, { stdio: 'inherit' });
