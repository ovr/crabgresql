--
-- INET
-- inet: IPv4/IPv6 input and output (the `/bits` suffix appears only when the
-- mask is not the full host mask), casts, network_cmp ordering, the
-- containment/overlap/bitwise operators, host arithmetic, and the field
-- functions. Output hand-checked against PostgreSQL's aligned format.
--
-- typed-literal output; a full host mask prints no suffix
SELECT inet '192.168.1.5' AS host, inet '192.168.1.5/24' AS with_mask;
-- IPv6, including the compressed canonical form
SELECT inet '::1' AS loopback, inet '2001:db8::/32' AS net;
-- abbreviated IPv4 left-aligns the octets, but only with an explicit mask
SELECT inet '10/8' AS ten, inet '192.168/16' AS priv;

-- casts: text -> inet and inet -> text
SELECT '10.0.0.1/8'::inet;
SELECT (inet '10.0.0.1/8')::text;

-- network_cmp order: family first (IPv4 < IPv6), then prefix, then masklen
SELECT a FROM (VALUES
    (inet '10.0.0.0/16'),
    (inet '::1'),
    (inet '10.0.0.0/8'),
    (inet '10.0.0.5')
) AS t(a) ORDER BY 1;

-- containment and overlap operators
SELECT inet '10.1.2.3' << inet '10.0.0.0/8'    AS contained,
       inet '10.0.0.0/8' >> inet '10.1.2.3'    AS contains,
       inet '10.0.0.0/8' && inet '10.1.0.0/16' AS overlaps,
       inet '11.0.0.0/8' >> inet '10.1.2.3'    AS not_contains;

-- bitwise operators
SELECT ~ inet '192.168.1.6'                          AS complement,
       inet '192.168.1.6' & inet '255.255.255.0'     AS masked,
       inet '192.168.1.6' | inet '0.0.0.255'         AS filled;

-- host arithmetic: inet +/- int and inet - inet
SELECT inet '10.0.0.1' + 5      AS plus,
       inet '10.0.0.10' - 4     AS minus,
       inet '10.0.0.10' - inet '10.0.0.1' AS distance;

-- field functions (a cidr argument coerces to the inet overloads)
SELECT host(inet '10.0.0.5/8')     AS host,
       masklen(inet '10.0.0.5/8')  AS masklen,
       family(inet '10.0.0.5')     AS fam4,
       family(inet '::1')          AS fam6,
       network(inet '192.168.1.5/24') AS network,
       abbrev(inet '10.0.0.5/8')   AS abbrev;

-- invalid input is 22P02
SELECT inet '999.1.1.1';
SELECT inet '10.0.0.1/33';
