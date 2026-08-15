import { open, SqlError } from '../src/index.js';

const db = open('/pgdata');
const sql = document.querySelector('#sql');
const out = document.querySelector('#out');

function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
}

function renderResult(result) {
  const fragment = document.createDocumentFragment();
  if (result.columns.length > 0) {
    const table = element('table');
    const header = element('tr');
    for (const column of result.columns) header.append(element('th', column));
    table.append(header);
    for (const row of result.rows) {
      const tr = element('tr');
      for (const value of row) {
        tr.append(
          value === null ? element('td', 'NULL', 'null') : element('td', value),
        );
      }
      table.append(tr);
    }
    fragment.append(table);
  }
  fragment.append(element('div', result.command, 'tag'));
  return fragment;
}

function run() {
  out.replaceChildren();
  const started = performance.now();
  try {
    const { results, notices } = db.query(sql.value);
    for (const result of results) out.append(renderResult(result));
    for (const notice of notices) {
      out.append(element('div', JSON.parse(notice).message, 'tag'));
    }
    out.append(
      element(
        'div',
        `${results.length} statement(s) in ${(performance.now() - started).toFixed(1)} ms`,
        'tag',
      ),
    );
  } catch (error) {
    out.append(
      element(
        'div',
        error instanceof SqlError
          ? `ERROR ${error.sqlstate}: ${error.message}${error.hint ? `\nHINT: ${error.hint}` : ''}`
          : String(error),
        'error',
      ),
    );
  }
}

document.querySelector('#run').addEventListener('click', run);
sql.addEventListener('keydown', (event) => {
  if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') run();
});

run();
