--
-- CIDR
-- cidr: a network specification. Input canonicalizes (masks host bits, infers
-- the masklen from the octet count) and always prints the `/bits` suffix; host
-- bits set to the right of the mask are rejected. Output hand-checked against
-- PostgreSQL's aligned format.
--
-- typed-literal output; the masklen is inferred from the octets written
SELECT cidr '192.168.1' AS three_octets, cidr '10' AS one_octet;
-- the bare masklen follows the first octet's historical class, widened to the
-- octets written (class B/C/E are not a flat octets*8)
SELECT cidr '128' AS class_b, cidr '192' AS class_c, cidr '240' AS class_e;
-- an explicit mask, and IPv6
SELECT cidr '192.168.100.0/24' AS v4, cidr '2001:db8::/32' AS v6;

-- casts: text -> cidr and cidr -> text
SELECT '10.1.0.0/16'::cidr;
SELECT (cidr '10.1.0.0/16')::text;

-- ordering (network_cmp)
SELECT c FROM (VALUES
    (cidr '10.0.0.0/16'),
    (cidr '10.0.0.0/8'),
    (cidr '192.168.0.0/16')
) AS t(c) ORDER BY 1;

-- field functions on cidr
SELECT masklen(cidr '10.1.0.0/16') AS masklen,
       family(cidr '10.1.0.0/16')  AS family,
       network(cidr '10.1.2.0/24') AS network,
       abbrev(cidr '10.1.0.0/16')  AS abbrev;
-- abbrev on IPv6 truncates the address to the masked prefix
SELECT abbrev(cidr '2001:db8::/32')     AS a32,
       abbrev(cidr '2001:db8:0:1::/64') AS a64;

-- host bits set to the right of the mask are rejected, with DETAIL
SELECT cidr '192.168.1.1/24';
