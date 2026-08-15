/**
 * CrabgreSQL in the browser (and in Node): a thin wrapper over the transpiled
 * component.
 *
 * The component's own API is deliberately narrow — a SQL string in, JSON out —
 * so everything that makes it pleasant to use from JS lives here: parsing the
 * result, and turning a failed statement into a thrown `SqlError` carrying its
 * SQLSTATE.
 *
 * The data directory lives in the in-memory filesystem from `host.js`, which
 * means a database lasts exactly as long as the page does.
 */
import { engine } from '../dist/crabgresql.js';
import { resetFilesystem, setEnvironment } from './host.js';

export { resetFilesystem, setEnvironment };

/** A statement the server refused, with the fields it refused it with. */
export class SqlError extends Error {
  constructor({ sqlstate, message, detail, hint, position, severity }) {
    super(message);
    this.name = 'SqlError';
    this.sqlstate = sqlstate;
    this.detail = detail;
    this.hint = hint;
    this.position = position;
    this.severity = severity;
  }
}

/**
 * Open a database at `dataDir`, creating and recovering it as the native server
 * would.
 */
export function open(dataDir = '/pgdata') {
  return new Database(new engine.Connection(dataDir));
}

export class Database {
  #connection;

  constructor(connection) {
    this.#connection = connection;
  }

  /**
   * Run one statement, or several separated by `;`.
   *
   * Returns `{results, notices}`, where each result is
   * `{columns, rows, command}` — `rows` being arrays of strings in the same
   * text encoding the wire protocol uses, with `null` for SQL NULL.
   */
  query(sql) {
    let json;
    try {
      json = this.#connection.exec(sql);
    } catch (error) {
      // A statement error arrives as the WIT error case, which jco throws.
      // Anything that is not our JSON — a trap, say — is rethrown untouched.
      const fields = parseError(error);
      if (fields === null) throw error;
      throw new SqlError(fields);
    }
    return JSON.parse(json);
  }

  /**
   * The first result's rows as objects keyed by column name — the shape most
   * callers want for a single `SELECT`.
   */
  rows(sql) {
    const [result] = this.query(sql).results;
    if (!result) return [];
    return result.rows.map((row) =>
      Object.fromEntries(result.columns.map((column, i) => [column, row[i]])),
    );
  }

  /** Flush everything to the data directory. */
  checkpoint() {
    this.#connection.checkpoint();
  }

  /** End the session. The data directory stays where it is. */
  close() {
    this.#connection[Symbol.dispose]();
  }
}

function parseError(error) {
  const payload = error?.payload ?? error;
  if (typeof payload !== 'string') return null;
  try {
    const fields = JSON.parse(payload);
    return typeof fields?.sqlstate === 'string' ? fields : null;
  } catch {
    return null;
  }
}
