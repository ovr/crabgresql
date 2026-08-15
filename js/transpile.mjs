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
import { readFileSync } from 'node:fs';

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
  // Not imported by today's component; mapped so that the day some dependency
  // reaches for it, it reaches ours. An unused mapping costs nothing.
  'wasi:random/insecure': 'insecure',
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

// The mappings above are matched by exact name *and version*. Anything missed —
// an interface nobody listed, or every interface at once after a WASI version
// bump past the hard-coded 0.2.9 — is not an error to jco: it silently falls
// back to `@bytecodealliance/preview2-shim`, whose filesystem is a different
// filesystem from ours. The component would then create its data directory in
// one and look for it in the other. So check what the output actually imports.
const generated = readFileSync('dist/crabgresql.js', 'utf8');
const imports = [...generated.matchAll(/^import .* from '([^']+)';$/gm)].map(
  (match) => match[1],
);
const strays = [...new Set(imports)].filter((from) => from !== '../src/host.js');
if (strays.length > 0) {
  throw new Error(
    `the transpiled component imports a host we did not supply: ${strays.join(', ')}\n` +
      `Add the missing interface to HOST in transpile.mjs (and implement it in ` +
      `src/host.js); if every interface is listed, WASI has moved past ${WASI}.`,
  );
}

