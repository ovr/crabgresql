//! `inet` / `cidr`: parsing, output, comparison, the containment/bitwise
//! operators, host arithmetic, and the field functions
//! (`host`/`masklen`/`family`/`network`/`abbrev`).
//!
//! Clean-room (see AGENTS.md): this reproduces PostgreSQL's *observable*
//! behavior — the canonical output text, the accepted abbreviated input, the
//! `network_cmp` order, and the SQLSTATE/message of syntax and range errors —
//! implemented independently.
//!
//! Representation: [`Inet`] holds the family flag, the 16-byte address (IPv4 in
//! the first four bytes), and the netmask/prefix length in bits. `inet` and
//! `cidr` share this struct; the difference is the type OID, whether host bits
//! are required to be zero, and the output rule for the `/bits` suffix.

use std::cmp::Ordering;
use std::net::{Ipv4Addr, Ipv6Addr};

// SQLSTATEs (kept as literals; the types crate does not depend on the protocol
// crate — the binder/executor map these to `sqlstate::*`).
const INVALID_TEXT_REPRESENTATION: &str = "22P02";
const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
const DATA_EXCEPTION: &str = "22000";

/// An `inet`/`cidr` value: family, 16-byte address (IPv4 in bytes 0..4), and
/// masklen in bits.
#[derive(deepsize::DeepSizeOf, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inet {
    pub is_ipv6: bool,
    pub addr: [u8; 16],
    pub bits: u8,
}

/// A parse/operation error carrying the SQLSTATE, message, and optional DETAIL
/// line PG reports.
#[derive(Clone, Debug, PartialEq)]
pub struct NetError {
    pub sqlstate: &'static str,
    pub message: String,
    pub detail: Option<&'static str>,
}

fn invalid_inet(input: &str) -> NetError {
    NetError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type inet: \"{input}\""),
        detail: None,
    }
}

fn invalid_cidr(input: &str) -> NetError {
    NetError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid input syntax for type cidr: \"{input}\""),
        detail: None,
    }
}

fn cidr_bits_set(input: &str) -> NetError {
    // PG reports the primary "invalid cidr value" plus a DETAIL line.
    NetError {
        sqlstate: INVALID_TEXT_REPRESENTATION,
        message: format!("invalid cidr value: \"{input}\""),
        detail: Some("Value has bits set to right of mask."),
    }
}

impl Inet {
    fn max_bits(&self) -> u8 {
        if self.is_ipv6 { 128 } else { 32 }
    }

    fn byte_len(&self) -> usize {
        if self.is_ipv6 { 16 } else { 4 }
    }

    /// Zero every host bit (right of `bits`), yielding the network address.
    fn masked(&self) -> Inet {
        let mut out = *self;
        let len = out.byte_len();
        apply_mask(&mut out.addr, out.bits, len);
        out
    }
}

/// Zero the bits right of `bits` across the first `len` bytes.
fn apply_mask(addr: &mut [u8; 16], bits: u8, len: usize) {
    let bits = bits as usize;
    for (i, byte) in addr.iter_mut().enumerate().take(len) {
        let hi = i * 8;
        if bits >= hi + 8 {
            // whole byte is inside the mask
        } else if bits <= hi {
            *byte = 0;
        } else {
            let keep = bits - hi; // 1..=7 leading bits kept
            *byte &= 0xffu8 << (8 - keep);
        }
    }
}

/// True if any host bit (right of `bits`) is set.
fn has_host_bits(addr: &[u8; 16], bits: u8, len: usize) -> bool {
    let mut masked = *addr;
    apply_mask(&mut masked, bits, len);
    masked[..len] != addr[..len]
}

/// Parse 1..4 dotted decimal octets, PG-style: fewer than four octets left-align
/// (`10` → 10.0.0.0, `192.168` → 192.168.0.0). Returns the four address bytes
/// and the count of octets written (which sets a `cidr`'s default masklen).
fn parse_ipv4(s: &str) -> Option<([u8; 4], usize)> {
    let mut out = [0u8; 4];
    let mut n = 0;
    for part in s.split('.') {
        // Each octet is 1..=3 decimal digits. PG's input treats leading zeros as
        // decimal (`001` → 1, not octal) and accepts no sign/other characters, so
        // reject anything that is not a bare run of ASCII digits.
        if n >= 4 || part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let v: u16 = part.parse().ok()?;
        if v > 255 {
            return None;
        }
        out[n] = v as u8;
        n += 1;
    }
    if n == 0 {
        return None;
    }
    Some((out, n))
}

/// PG's default masklen for a bare `cidr` (no `/bits`): the historical class of
/// the first octet, widened to cover the octets actually written. Reproduces
/// PG's observable output for `cidr '128'` (/16), `cidr '192'` (/24), etc.
/// (The lone multicast-base literal `cidr '224'` is PG's one `/4` special case,
/// not modeled here.)
fn cidr_default_bits(first: u8, octets: usize) -> u8 {
    let classful: u8 = match first {
        0..=127 => 8,    // class A
        128..=191 => 16, // class B
        192..=223 => 24, // class C
        224..=239 => 4,  // class D (multicast)
        240..=255 => 32, // class E
    };
    classful.max((octets * 8) as u8)
}

/// Shared body of `inet_in`/`cidr_in`. `cidr` selects the stricter host-bits
/// check and default masking.
fn parse_common(input: &str, cidr: bool) -> Result<Inet, NetError> {
    let invalid = |s: &str| {
        if cidr {
            invalid_cidr(s)
        } else {
            invalid_inet(s)
        }
    };
    let (addr_str, mask_str) = match input.split_once('/') {
        Some((a, m)) => (a, Some(m)),
        None => (input, None),
    };

    let (is_ipv6, addr, octets) = if addr_str.contains(':') {
        let v6: Ipv6Addr = addr_str.parse().map_err(|_| invalid(input))?;
        (true, v6.octets(), 16usize)
    } else {
        let (v4, n) = parse_ipv4(addr_str).ok_or_else(|| invalid(input))?;
        // A bare `inet` (no `/bits`) requires a full four-octet address; the
        // abbreviated left-aligned form is only accepted with a mask, or for
        // `cidr` (which infers the masklen from the octet count).
        if !cidr && mask_str.is_none() && n < 4 {
            return Err(invalid(input));
        }
        let mut a = [0u8; 16];
        a[..4].copy_from_slice(&v4);
        (false, a, n)
    };

    let max = if is_ipv6 { 128u8 } else { 32u8 };
    let bits = match mask_str {
        Some(m) => {
            // The mask is a bare run of decimal digits (no sign), 0..=max.
            if m.is_empty() || !m.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid(input));
            }
            let b: u8 = m.parse().map_err(|_| invalid(input))?;
            if b > max {
                return Err(invalid(input));
            }
            b
        }
        // A bare `cidr` infers its masklen from the first octet's historical
        // class, widened to the octets written (`10` → /8, `128` → /16,
        // `192.168.1` → /24); a bare `inet` uses the full host mask.
        None if cidr && !is_ipv6 => cidr_default_bits(addr[0], octets),
        None => max,
    };

    let len = if is_ipv6 { 16 } else { 4 };
    if cidr && has_host_bits(&addr, bits, len) {
        return Err(cidr_bits_set(input));
    }

    let mut v = Inet {
        is_ipv6,
        addr,
        bits,
    };
    if cidr {
        v = v.masked();
    }
    Ok(v)
}

/// `inet_in`.
pub fn inet_in(input: &str) -> Result<Inet, NetError> {
    parse_common(input, false)
}

/// `cidr_in`.
pub fn cidr_in(input: &str) -> Result<Inet, NetError> {
    parse_common(input, true)
}

/// The bare address (no `/bits`) in canonical form: dotted quad or the RFC 5952
/// compressed IPv6 form (`std`'s `Ipv6Addr` display matches PG's).
fn addr_str(v: &Inet) -> String {
    if v.is_ipv6 {
        let mut o = [0u8; 16];
        o.copy_from_slice(&v.addr);
        Ipv6Addr::from(o).to_string()
    } else {
        Ipv4Addr::new(v.addr[0], v.addr[1], v.addr[2], v.addr[3]).to_string()
    }
}

/// `inet_out`: the address, plus `/bits` only when the mask is not the full
/// host mask (`/32` for IPv4, `/128` for IPv6).
pub fn inet_out(v: &Inet) -> String {
    if v.bits == v.max_bits() {
        addr_str(v)
    } else {
        format!("{}/{}", addr_str(v), v.bits)
    }
}

/// `cidr_out`: always the network address followed by `/bits`.
pub fn cidr_out(v: &Inet) -> String {
    format!("{}/{}", addr_str(&v.masked()), v.bits)
}

/// The `inet -> text` *cast*, which is not [`inet_out`]: PG's dedicated cast
/// always spells the masklen out, so `192.168.1.5` casts to `192.168.1.5/32`
/// even though it displays without one. (`cidr` needs no equivalent —
/// [`cidr_out`] already always prints the masklen.)
pub fn inet_text(v: &Inet) -> String {
    format!("{}/{}", addr_str(v), v.bits)
}

/// Compare the first `nbits` bits of two addresses.
fn bitncmp(a: &[u8; 16], b: &[u8; 16], nbits: u8) -> Ordering {
    let whole = (nbits / 8) as usize;
    let rem = nbits % 8;
    let ord = a[..whole].cmp(&b[..whole]);
    if ord != Ordering::Equal {
        return ord;
    }
    if rem == 0 {
        return Ordering::Equal;
    }
    let m = 0xffu8 << (8 - rem);
    (a[whole] & m).cmp(&(b[whole] & m))
}

/// `network_cmp`: order by family, then the common-prefix bits, then masklen,
/// then the full address. Total order used by `<`, `ORDER BY`, indexes.
pub fn network_cmp(a: &Inet, b: &Inet) -> Ordering {
    a.is_ipv6
        .cmp(&b.is_ipv6)
        .then_with(|| bitncmp(&a.addr, &b.addr, a.bits.min(b.bits)))
        .then_with(|| a.bits.cmp(&b.bits))
        .then_with(|| a.addr[..a.byte_len()].cmp(&b.addr[..a.byte_len()]))
}

// ---- containment / overlap operators ----

/// `a >> b` — `a` strictly contains `b` (`network_sup`).
pub fn contains(a: &Inet, b: &Inet) -> bool {
    a.is_ipv6 == b.is_ipv6
        && a.bits < b.bits
        && bitncmp(&a.addr, &b.addr, a.bits) == Ordering::Equal
}

/// `a << b` — `a` is strictly contained by `b`.
pub fn contained_by(a: &Inet, b: &Inet) -> bool {
    contains(b, a)
}

/// `a && b` — the two networks overlap (either contains the other).
pub fn overlaps(a: &Inet, b: &Inet) -> bool {
    a.is_ipv6 == b.is_ipv6 && bitncmp(&a.addr, &b.addr, a.bits.min(b.bits)) == Ordering::Equal
}

// ---- bitwise operators ----

fn family_mismatch(op: &str) -> NetError {
    NetError {
        sqlstate: DATA_EXCEPTION,
        message: format!("cannot {op} inet values of different sizes"),
        detail: None,
    }
}

/// `~inet` (`inetnot`): bitwise NOT of the address, masklen preserved.
pub fn bit_not(a: &Inet) -> Inet {
    let mut out = *a;
    for i in 0..a.byte_len() {
        out.addr[i] = !a.addr[i];
    }
    out
}

fn bit_binop(a: &Inet, b: &Inet, op: &str, f: impl Fn(u8, u8) -> u8) -> Result<Inet, NetError> {
    if a.is_ipv6 != b.is_ipv6 {
        return Err(family_mismatch(op));
    }
    let mut out = *a;
    for i in 0..a.byte_len() {
        out.addr[i] = f(a.addr[i], b.addr[i]);
    }
    out.bits = a.bits.max(b.bits);
    Ok(out)
}

/// `inet & inet` (`inetand`).
pub fn bit_and(a: &Inet, b: &Inet) -> Result<Inet, NetError> {
    bit_binop(a, b, "AND", |x, y| x & y)
}

/// `inet | inet` (`inetor`).
pub fn bit_or(a: &Inet, b: &Inet) -> Result<Inet, NetError> {
    bit_binop(a, b, "OR", |x, y| x | y)
}

// ---- host arithmetic ----

fn out_of_range() -> NetError {
    NetError {
        sqlstate: NUMERIC_VALUE_OUT_OF_RANGE,
        message: "result is out of range".to_string(),
        detail: None,
    }
}

fn addr_to_u128(v: &Inet) -> u128 {
    let mut n = 0u128;
    for &byte in &v.addr[..v.byte_len()] {
        n = (n << 8) | byte as u128;
    }
    n
}

fn u128_to_addr(mut n: u128, template: &Inet) -> Inet {
    let mut out = *template;
    let len = template.byte_len();
    for i in (0..len).rev() {
        out.addr[i] = (n & 0xff) as u8;
        n >>= 8;
    }
    out
}

/// Shift the host address by `mag` (subtracting when `neg`), preserving family
/// and masklen. Out-of-family-range is `22003 result is out of range`. Uses
/// unsigned arithmetic so the full IPv6 range and `i64::MIN` are both safe.
fn shift(a: &Inet, neg: bool, mag: u128) -> Result<Inet, NetError> {
    let base = addr_to_u128(a);
    let result = if neg {
        base.checked_sub(mag)
    } else {
        base.checked_add(mag)
    }
    .ok_or_else(out_of_range)?;
    if !a.is_ipv6 && result > 0xffff_ffff {
        return Err(out_of_range());
    }
    Ok(u128_to_addr(result, a))
}

/// `inet + int8`: shift the host address up by `offset`.
pub fn add_offset(a: &Inet, offset: i64) -> Result<Inet, NetError> {
    shift(a, offset < 0, offset.unsigned_abs() as u128)
}

/// `inet - int8`: shift the host address down by `offset`.
pub fn sub_offset(a: &Inet, offset: i64) -> Result<Inet, NetError> {
    shift(a, offset >= 0, offset.unsigned_abs() as u128)
}

/// `inet - inet`: the signed distance as int8. Different families is an error;
/// a difference outside int8 is `22003 result is out of range`.
pub fn diff(a: &Inet, b: &Inet) -> Result<i64, NetError> {
    if a.is_ipv6 != b.is_ipv6 {
        return Err(NetError {
            sqlstate: DATA_EXCEPTION,
            message: "cannot subtract inet values of different sizes".to_string(),
            detail: None,
        });
    }
    let (av, bv) = (addr_to_u128(a), addr_to_u128(b));
    // Compute |a - b| unsigned, then apply the sign — avoids the i128 cast
    // truncating IPv6 addresses that overflow i128's positive range.
    if av >= bv {
        i64::try_from(av - bv).map_err(|_| out_of_range())
    } else {
        // The negative side reaches one further than `i64::MAX`: a magnitude of
        // exactly 2^63 is the valid result `i64::MIN`. `(2^63 as i64)` wraps to
        // `i64::MIN`, whose `wrapping_neg` is itself, giving the right value.
        let d = bv - av;
        if d <= 1u128 << 63 {
            Ok((d as i64).wrapping_neg())
        } else {
            Err(out_of_range())
        }
    }
}

// ---- field functions ----

/// `host(inet)`: the address text, never with a `/bits` suffix.
pub fn host(v: &Inet) -> String {
    addr_str(v)
}

/// `masklen(inet)`.
pub fn masklen(v: &Inet) -> i32 {
    v.bits as i32
}

/// `family(inet)`: 4 or 6.
pub fn family(v: &Inet) -> i32 {
    if v.is_ipv6 { 6 } else { 4 }
}

/// `network(inet)`: the network part as a `cidr` (address masked to `bits`).
pub fn network(v: &Inet) -> Inet {
    v.masked()
}

/// `abbrev(inet)`: the inet text (address, plus `/bits` unless the full mask).
pub fn abbrev_inet(v: &Inet) -> String {
    inet_out(v)
}

/// `abbrev(cidr)`: the shortest unambiguous form of the network, dropping the
/// address portion beyond the mask (`10.1.0.0/16` → `10.1/16`,
/// `2001:db8::/32` → `2001:db8/32`), then the `/bits` suffix.
pub fn abbrev_cidr(v: &Inet) -> String {
    let net = v.masked();
    let addr = if net.is_ipv6 {
        abbrev_ipv6(&net.addr, net.bits)
    } else {
        // Keep only the octets covered by the mask (rounded up), min one.
        let octets = (net.bits as usize).div_ceil(8).max(1);
        net.addr[..octets]
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(".")
    };
    format!("{}/{}", addr, net.bits)
}

/// PG's `inet_cidr_ntop` IPv6 rendering: show only the `ceil(bits/16)` groups
/// covered by the mask. A zero-length prefix is `::`; a single group appends
/// `::`; two or more groups use the standard `::` zero-run compression.
fn abbrev_ipv6(addr: &[u8; 16], bits: u8) -> String {
    let n = (bits as usize).div_ceil(16);
    let group = |i: usize| ((addr[i * 2] as u16) << 8) | addr[i * 2 + 1] as u16;
    if n == 0 {
        return "::".to_string();
    }
    if n == 1 {
        return format!("{:x}::", group(0));
    }
    let words: Vec<u16> = (0..n).map(group).collect();
    // Longest run of zero groups (length ≥ 1), leftmost on ties.
    let (mut best_start, mut best_len) = (0usize, 0usize);
    let mut i = 0;
    while i < words.len() {
        if words[i] == 0 {
            let start = i;
            while i < words.len() && words[i] == 0 {
                i += 1;
            }
            if i - start > best_len {
                best_len = i - start;
                best_start = start;
            }
        } else {
            i += 1;
        }
    }
    let hex = |ws: &[u16]| {
        ws.iter()
            .map(|w| format!("{w:x}"))
            .collect::<Vec<_>>()
            .join(":")
    };
    if best_len == 0 {
        hex(&words)
    } else {
        format!(
            "{}::{}",
            hex(&words[..best_start]),
            hex(&words[best_start + best_len..])
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inet(s: &str) -> Inet {
        match inet_in(s) {
            Ok(value) => value,
            Err(error) => panic!("invalid inet test fixture `{s}`: {error:?}"),
        }
    }
    fn cidr(s: &str) -> Inet {
        match cidr_in(s) {
            Ok(value) => value,
            Err(error) => panic!("invalid cidr test fixture `{s}`: {error:?}"),
        }
    }

    #[test]
    fn ipv4_roundtrip() {
        assert_eq!(inet_out(&inet("192.168.1.5")), "192.168.1.5");
        assert_eq!(inet_out(&inet("192.168.1.5/24")), "192.168.1.5/24");
        assert_eq!(inet_out(&inet("10/8")), "10.0.0.0/8");
        // A bare inet (no mask) requires all four octets.
        assert_eq!(
            inet_in("192.168")
                .expect_err("a maskless inet needs all four octets, and `192.168` has two")
                .sqlstate,
            "22P02"
        );
    }

    #[test]
    fn ipv6_roundtrip() {
        assert_eq!(inet_out(&inet("::1")), "::1");
        assert_eq!(inet_out(&inet("2001:db8::/32")), "2001:db8::/32");
    }

    #[test]
    fn cidr_canonicalizes_and_rejects_host_bits() {
        assert_eq!(cidr_out(&cidr("192.168.1")), "192.168.1.0/24");
        assert_eq!(cidr_out(&cidr("10/8")), "10.0.0.0/8");
        let e = cidr_in("192.168.1.1/24")
            .expect_err("a cidr may not carry host bits, and .1 sits right of the /24");
        assert_eq!(e.sqlstate, "22P02");
        assert_eq!(e.message, "invalid cidr value: \"192.168.1.1/24\"");
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(
            inet_in("999.1.1.1")
                .expect_err("999 is past the 255 an octet holds")
                .sqlstate,
            "22P02"
        );
        assert_eq!(
            inet_in("1.2.3.4/33")
                .expect_err("an IPv4 masklen stops at 32, so /33 is out of range")
                .sqlstate,
            "22P02"
        );
        assert_eq!(
            inet_in("garbage")
                .expect_err("`garbage` is neither dotted quad nor IPv6")
                .sqlstate,
            "22P02"
        );
        // A leading '+' is rejected in both octets and the mask (Rust's parse
        // would otherwise accept it, unlike PG).
        assert_eq!(
            inet_in("+1.2.3.4")
                .expect_err("a signed leading octet is not an address")
                .sqlstate,
            "22P02"
        );
        assert_eq!(
            inet_in("1.2.3.4/+8")
                .expect_err("a signed masklen is not a bare digit run")
                .sqlstate,
            "22P02"
        );
    }

    #[test]
    fn leading_zero_octets_are_decimal() {
        // PG treats leading zeros as decimal, not octal, and accepts them.
        assert_eq!(inet_out(&inet("192.168.001.1")), "192.168.1.1");
        assert_eq!(inet_out(&inet("010.0.0.1")), "10.0.0.1");
    }

    #[test]
    fn cidr_default_masklen_is_classful() {
        // A bare cidr's masklen follows the first octet's class, widened to the
        // octets written (not a flat octets*8).
        assert_eq!(cidr_out(&cidr("10")), "10.0.0.0/8");
        assert_eq!(cidr_out(&cidr("128")), "128.0.0.0/16");
        assert_eq!(cidr_out(&cidr("192")), "192.0.0.0/24");
        assert_eq!(cidr_out(&cidr("240")), "240.0.0.0/32");
        assert_eq!(cidr_out(&cidr("10.1")), "10.1.0.0/16");
        assert_eq!(cidr_out(&cidr("192.168.1")), "192.168.1.0/24");
    }

    #[test]
    fn abbrev_ipv6_truncates_to_mask() {
        assert_eq!(abbrev_cidr(&cidr("2001:db8::/32")), "2001:db8/32");
        assert_eq!(abbrev_cidr(&cidr("2001:db8:0:1::/64")), "2001:db8::1/64");
        assert_eq!(abbrev_cidr(&cidr("ffff::/16")), "ffff::/16");
        assert_eq!(
            abbrev_cidr(&cidr("2001:db8:1:2:3::/80")),
            "2001:db8:1:2:3/80"
        );
        assert_eq!(abbrev_cidr(&cidr("::/0")), "::/0");
        assert_eq!(abbrev_cidr(&cidr("0:1::/32")), "::1/32");
    }

    #[test]
    fn diff_reaches_i64_min() -> anyhow::Result<()> {
        // A negative difference of exactly 2^63 is the valid result i64::MIN.
        let a = inet("::");
        let b = inet("::8000:0:0:0"); // 2^63
        assert_eq!(diff(&a, &b)?, i64::MIN);
        // One past that overflows.
        let c = inet("::8000:0:0:1");
        assert_eq!(
            diff(&a, &c)
                .expect_err("a negative distance of 2^63 + 1 is one past i64::MIN")
                .sqlstate,
            "22003"
        );

        Ok(())
    }

    #[test]
    fn ordering_family_then_prefix() {
        // IPv4 sorts before IPv6; shorter masklen sorts first at equal prefix.
        assert_eq!(
            network_cmp(&inet("10.0.0.0/8"), &inet("::1")),
            Ordering::Less
        );
        assert_eq!(
            network_cmp(&inet("10.0.0.0/8"), &inet("10.0.0.0/16")),
            Ordering::Less
        );
        assert_eq!(
            network_cmp(&inet("10.0.0.1"), &inet("10.0.0.2")),
            Ordering::Less
        );
    }

    #[test]
    fn containment() {
        assert!(contains(&inet("10.0.0.0/8"), &inet("10.1.2.3")));
        assert!(contained_by(&inet("10.1.2.3"), &inet("10.0.0.0/8")));
        assert!(!contains(&inet("10.0.0.0/8"), &inet("11.1.2.3")));
        assert!(overlaps(&inet("10.0.0.0/8"), &inet("10.1.0.0/16")));
    }

    #[test]
    fn bitwise_and_arith() -> anyhow::Result<()> {
        assert_eq!(inet_out(&bit_not(&inet("192.168.1.6"))), "63.87.254.249");
        assert_eq!(
            inet_out(&bit_and(&inet("192.168.1.6"), &inet("255.255.255.0"))?),
            "192.168.1.0"
        );
        assert_eq!(inet_out(&add_offset(&inet("10.0.0.1"), 5)?), "10.0.0.6");
        assert_eq!(diff(&inet("10.0.0.10"), &inet("10.0.0.1"))?, 9);
        assert_eq!(
            add_offset(&inet("255.255.255.255"), 1)
                .expect_err("stepping one past the last IPv4 address leaves the family")
                .sqlstate,
            "22003"
        );

        Ok(())
    }

    #[test]
    fn field_functions() {
        assert_eq!(host(&inet("10.0.0.1/8")), "10.0.0.1");
        assert_eq!(masklen(&inet("10.0.0.1/8")), 8);
        assert_eq!(family(&inet("10.0.0.1")), 4);
        assert_eq!(family(&inet("::1")), 6);
        assert_eq!(
            cidr_out(&network(&inet("192.168.1.5/24"))),
            "192.168.1.0/24"
        );
        assert_eq!(abbrev_cidr(&cidr("10.1.0.0/16")), "10.1/16");
    }
}
