/**
 * Drives the transpiled component under Node against the same in-memory host
 * the browser uses, so the wasm build is exercised end-to-end without a
 * browser: DDL, insert, read back, transaction rollback, and a deliberate
 * failure to prove errors arrive as SQLSTATEs rather than as traps.
 *
 * Run after `npm run build`.
 */
import { open, SqlError } from './src/index.js';

let failures = 0;

function check(what, actual, expected) {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) {
    failures += 1;
    console.log(`     expected ${JSON.stringify(expected)}`);
    console.log(`     got      ${JSON.stringify(actual)}`);
  }
}

const db = open('/pgdata');

check('select 1', db.rows('select 1 as one'), [{ one: '1' }]);

db.query('create table t(a int, b text)');
db.query("insert into t values (1, 'one'), (2, null)");
check('rows read back', db.rows('select a, b from t order by a'), [
  { a: '1', b: 'one' },
  { a: '2', b: null },
]);

const tags = db
  .query('select 1; select 2')
  .results.map((result) => result.command);
check('multi-statement tags', tags, ['SELECT 1', 'SELECT 1']);

db.query('begin');
db.query('insert into t values (3, null)');
db.query('rollback');
check('rollback discards the insert', db.rows('select count(*) from t'), [
  { count: '2' },
]);

// 4000 rows is well past the 64 KiB pipe the session runs over, and past what
// the buffer holds without flushing — the two places a wasm build could quietly
// truncate or deadlock.
db.query("insert into t select g, repeat('x', 100) from generate_series(3, 4002) g");
check('bulk insert', db.rows('select count(*) from t'), [{ count: '4002' }]);

try {
  db.query('select * from missing_table');
  check('missing table raises', 'no error', 'an error');
} catch (error) {
  check(
    'missing table raises 42P01',
    error instanceof SqlError ? error.sqlstate : String(error),
    '42P01',
  );
}

db.checkpoint();
console.log(
  `\n${failures === 0 ? 'all checks passed' : `${failures} check(s) failed`}`,
);
process.exit(failures === 0 ? 0 : 1);
