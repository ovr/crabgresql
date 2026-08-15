/**
 * Drives the transpiled component under Node against the same in-memory host
 * the browser uses, so the wasm build is exercised end-to-end without a
 * browser: DDL, insert, read back, transaction rollback, and a deliberate
 * failure to prove errors arrive as SQLSTATEs rather than as traps.
 *
 * Run after `npm run build`.
 */
import { open, resetFilesystem, SqlError } from './src/index.js';

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

// `drop table if exists` on a missing table is the cheapest NOTICE there is;
// it must arrive as an object, not as JSON hiding inside a string.
const { notices } = db.query('drop table if exists never_existed');
check(
  'a notice is an object',
  notices.map((notice) => notice.severity),
  ['NOTICE'],
);

// The one that used to freeze the tab: nothing can send CopyData, so the server
// must be told so instead of waited on. A hang here *is* the regression.
try {
  db.query('copy t from stdin');
  check('copy from stdin is refused', 'no error', 'an error');
} catch (error) {
  check(
    'copy from stdin is refused, not awaited',
    error instanceof SqlError && error.message.includes('COPY FROM STDIN'),
    true,
  );
}
check('the session survives a refused copy', db.rows('select 1 as ok'), [
  { ok: '1' },
]);

// Two engines over one data directory would be two WALs on one log.
let secondOpen = 'no error';
try {
  open('/pgdata');
} catch (error) {
  secondOpen = String(error.payload ?? error).includes('already open')
    ? 'refused'
    : String(error);
}
check('a second open of the same directory', secondOpen, 'refused');

db.checkpoint();
check('the database runs after a checkpoint', db.rows('select 1 as ok'), [
  { ok: '1' },
]);

// A reset has to be visible through the preopen the guest is already holding,
// which is why it clears the tree in place.
db.close();
resetFilesystem();
const fresh = open('/pgdata');
check(
  'the reset filesystem has no tables',
  fresh.rows(
    "select count(*) from pg_catalog.pg_class where relname = 't'",
  ),
  [{ count: '0' }],
);
console.log(
  `\n${failures === 0 ? 'all checks passed' : `${failures} check(s) failed`}`,
);
process.exit(failures === 0 ? 0 : 1);
