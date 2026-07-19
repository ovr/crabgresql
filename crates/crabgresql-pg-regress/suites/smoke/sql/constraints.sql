-- CONSTRAINTS AND SEMANTIC INDEXES
CREATE TABLE constraint_demo (
  id integer PRIMARY KEY,
  code integer UNIQUE,
  required text NOT NULL,
  amount integer DEFAULT (1 + 2)
);
INSERT INTO constraint_demo (id, required) VALUES (1, 'one');
INSERT INTO constraint_demo VALUES (2, NULL, 'two', DEFAULT);
UPDATE constraint_demo SET amount = DEFAULT WHERE id = 2;
SELECT id, code, required, amount FROM constraint_demo ORDER BY id;
CREATE INDEX constraint_demo_required_idx ON constraint_demo(required);
SELECT column_name, column_default, is_nullable
  FROM information_schema.columns
 WHERE table_name = 'constraint_demo'
 ORDER BY ordinal_position;
INSERT INTO constraint_demo (id, required) VALUES (1, 'duplicate');
INSERT INTO constraint_demo (id, required) VALUES (3, NULL);
