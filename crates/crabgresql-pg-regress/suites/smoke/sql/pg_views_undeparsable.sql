--
-- A view whose body this build cannot deparse
--
-- `ruleutils::view_definition` renders a single SELECT and nothing else, so a
-- view defined with UNION (or VALUES) can be created and queried but not
-- printed back. PostgreSQL deparses every view it can create, so it has no such
-- case — which is why the expected output here is this build's own, unlike
-- every other file in this suite. It exists to pin the behavior: the body is
-- reported as *absent*, never as a body that would not recreate the view.
--
-- When view_definition learns set operations, this file is what has to change,
-- and the diff will say so.
--
CREATE TABLE uv_t (a int);
INSERT INTO uv_t VALUES (1), (2);
CREATE VIEW uv_plain AS SELECT a FROM uv_t WHERE a > 0;
CREATE VIEW uv_union AS SELECT a FROM uv_t UNION SELECT 3;
-- The view itself works: only its *rendering* is missing.
SELECT * FROM uv_union ORDER BY a;
-- A deparsable view reports its body; the UNION one reports NULL rather than a
-- partial or invented definition.
SELECT viewname, definition IS NULL AS definition_missing
  FROM pg_views WHERE viewname LIKE 'uv\_%' ORDER BY viewname;
SELECT definition FROM pg_views WHERE viewname = 'uv_plain';
-- pg_rewrite agrees with pg_views: the _RETURN rule exists for both views, and
-- carries an action only for the one that can be rendered.
SELECT ev_class::regclass AS view, ev_action IS NULL AS action_missing
  FROM pg_rewrite WHERE ev_class IN ('uv_plain'::regclass, 'uv_union'::regclass)
 ORDER BY 1;
-- pg_get_viewdef raises rather than returning NULL, which is its answer for a
-- relation that is not a view at all — the two must not collide.
SELECT pg_get_viewdef('uv_union');
DROP VIEW uv_union;
DROP VIEW uv_plain;
DROP TABLE uv_t;
