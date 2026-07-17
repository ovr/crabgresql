//! `erf`, `erfc`, `tgamma`, `lgamma` for f64 — the special functions Rust's
//! `std` does not provide, needed by SQL `erf`/`erfc`/`gamma`/`lgamma`.
//!
//! Vendored (not a dependency on the whole `libm` crate) from the FreeBSD/msun
//! implementations as ported by the `libm` crate (github.com/rust-lang/libm).
//! Only the special-function kernels that have no `std` equivalent are copied
//! here; their internal `exp`/`pow`/`log`/`floor`/`fabs` calls are routed to
//! Rust `std` (which are IEEE-correct or high-quality on our targets). See
//! NOTICE for attribution.
//!
//! Original code:
//!   origin: FreeBSD /usr/src/lib/msun/src/{s_erf,e_lgamma_r}.c and
//!   the Lanczos tgamma from the libm crate.
//!
//!   ====================================================
//!   Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
//!   Developed at SunPro/SunSoft, a Sun Microsystems, Inc. business.
//!   Permission to use, copy, modify, and distribute this software is freely
//!   granted, provided that this notice is preserved.
//!   ====================================================

#![allow(clippy::all)]

use std::hint::black_box;

#[inline]
fn get_high_word(x: f64) -> u32 {
    (x.to_bits() >> 32) as u32
}

#[inline]
fn with_set_low_word(x: f64, lo: u32) -> f64 {
    f64::from_bits((x.to_bits() & 0xffff_ffff_0000_0000) | lo as u64)
}

// ===========================================================================
// erf / erfc  (origin: FreeBSD s_erf.c)
// ===========================================================================

const ERX: f64 = 8.45062911510467529297e-01;
const EFX8: f64 = 1.02703333676410069053e+00;
const PP0: f64 = 1.28379167095512558561e-01;
const PP1: f64 = -3.25042107247001499370e-01;
const PP2: f64 = -2.84817495755985104766e-02;
const PP3: f64 = -5.77027029648944159157e-03;
const PP4: f64 = -2.37630166566501626084e-05;
const QQ1: f64 = 3.97917223959155352819e-01;
const QQ2: f64 = 6.50222499887672944485e-02;
const QQ3: f64 = 5.08130628187576562776e-03;
const QQ4: f64 = 1.32494738004321644526e-04;
const QQ5: f64 = -3.96022827877536812320e-06;
const PA0: f64 = -2.36211856075265944077e-03;
const PA1: f64 = 4.14856118683748331666e-01;
const PA2: f64 = -3.72207876035701323847e-01;
const PA3: f64 = 3.18346619901161753674e-01;
const PA4: f64 = -1.10894694282396677476e-01;
const PA5: f64 = 3.54783043256182359371e-02;
const PA6: f64 = -2.16637559486879084300e-03;
const QA1: f64 = 1.06420880400844228286e-01;
const QA2: f64 = 5.40397917702171048937e-01;
const QA3: f64 = 7.18286544141962662868e-02;
const QA4: f64 = 1.26171219808761642112e-01;
const QA5: f64 = 1.36370839120290507362e-02;
const QA6: f64 = 1.19844998467991074170e-02;
const RA0: f64 = -9.86494403484714822705e-03;
const RA1: f64 = -6.93858572707181764372e-01;
const RA2: f64 = -1.05586262253232909814e+01;
const RA3: f64 = -6.23753324503260060396e+01;
const RA4: f64 = -1.62396669462573470355e+02;
const RA5: f64 = -1.84605092906711035994e+02;
const RA6: f64 = -8.12874355063065934246e+01;
const RA7: f64 = -9.81432934416914548592e+00;
const SA1: f64 = 1.96512716674392571292e+01;
const SA2: f64 = 1.37657754143519042600e+02;
const SA3: f64 = 4.34565877475229228821e+02;
const SA4: f64 = 6.45387271733267880336e+02;
const SA5: f64 = 4.29008140027567833386e+02;
const SA6: f64 = 1.08635005541779435134e+02;
const SA7: f64 = 6.57024977031928170135e+00;
const SA8: f64 = -6.04244152148580987438e-02;
const RB0: f64 = -9.86494292470009928597e-03;
const RB1: f64 = -7.99283237680523006574e-01;
const RB2: f64 = -1.77579549177547519889e+01;
const RB3: f64 = -1.60636384855821916062e+02;
const RB4: f64 = -6.37566443368389627722e+02;
const RB5: f64 = -1.02509513161107724954e+03;
const RB6: f64 = -4.83519191608651397019e+02;
const SB1: f64 = 3.03380607434824582924e+01;
const SB2: f64 = 3.25792512996573918826e+02;
const SB3: f64 = 1.53672958608443695994e+03;
const SB4: f64 = 3.19985821950859553908e+03;
const SB5: f64 = 2.55305040643316442583e+03;
const SB6: f64 = 4.74528541206955367215e+02;
const SB7: f64 = -2.24409524465858183362e+01;

fn erfc1(x: f64) -> f64 {
    let s = x.abs() - 1.0;
    let p = PA0 + s * (PA1 + s * (PA2 + s * (PA3 + s * (PA4 + s * (PA5 + s * PA6)))));
    let q = 1.0 + s * (QA1 + s * (QA2 + s * (QA3 + s * (QA4 + s * (QA5 + s * QA6)))));
    1.0 - ERX - p / q
}

fn erfc2(ix: u32, mut x: f64) -> f64 {
    if ix < 0x3ff40000 {
        return erfc1(x);
    }
    x = x.abs();
    let s = 1.0 / (x * x);
    let (r, big_s);
    if ix < 0x4006db6d {
        r = RA0 + s * (RA1 + s * (RA2 + s * (RA3 + s * (RA4 + s * (RA5 + s * (RA6 + s * RA7))))));
        big_s = 1.0
            + s * (SA1
                + s * (SA2 + s * (SA3 + s * (SA4 + s * (SA5 + s * (SA6 + s * (SA7 + s * SA8)))))));
    } else {
        r = RB0 + s * (RB1 + s * (RB2 + s * (RB3 + s * (RB4 + s * (RB5 + s * RB6)))));
        big_s =
            1.0 + s * (SB1 + s * (SB2 + s * (SB3 + s * (SB4 + s * (SB5 + s * (SB6 + s * SB7))))));
    }
    let z = with_set_low_word(x, 0);
    (-z * z - 0.5625).exp() * ((z - x) * (z + x) + r / big_s).exp() / x
}

pub fn erf(x: f64) -> f64 {
    let mut ix = get_high_word(x);
    let sign = (ix >> 31) as usize;
    ix &= 0x7fffffff;
    if ix >= 0x7ff00000 {
        return 1.0 - 2.0 * (sign as f64) + 1.0 / x;
    }
    if ix < 0x3feb0000 {
        if ix < 0x3e300000 {
            return 0.125 * (8.0 * x + EFX8 * x);
        }
        let z = x * x;
        let r = PP0 + z * (PP1 + z * (PP2 + z * (PP3 + z * PP4)));
        let s = 1.0 + z * (QQ1 + z * (QQ2 + z * (QQ3 + z * (QQ4 + z * QQ5))));
        let y = r / s;
        return x + x * y;
    }
    let y = if ix < 0x40180000 {
        1.0 - erfc2(ix, x)
    } else {
        let x1p_1022 = f64::from_bits(0x0010000000000000);
        1.0 - x1p_1022
    };
    if sign != 0 { -y } else { y }
}

pub fn erfc(x: f64) -> f64 {
    let mut ix = get_high_word(x);
    let sign = (ix >> 31) as usize;
    ix &= 0x7fffffff;
    if ix >= 0x7ff00000 {
        return 2.0 * (sign as f64) + 1.0 / x;
    }
    if ix < 0x3feb0000 {
        if ix < 0x3c700000 {
            return 1.0 - x;
        }
        let z = x * x;
        let r = PP0 + z * (PP1 + z * (PP2 + z * (PP3 + z * PP4)));
        let s = 1.0 + z * (QQ1 + z * (QQ2 + z * (QQ3 + z * (QQ4 + z * QQ5))));
        let y = r / s;
        if sign != 0 || ix < 0x3fd00000 {
            return 1.0 - (x + x * y);
        }
        return 0.5 - (x - 0.5 + x * y);
    }
    if ix < 0x403c0000 {
        return if sign != 0 {
            2.0 - erfc2(ix, x)
        } else {
            erfc2(ix, x)
        };
    }
    let x1p_1022 = f64::from_bits(0x0010000000000000);
    if sign != 0 {
        2.0 - x1p_1022
    } else {
        x1p_1022 * x1p_1022
    }
}

// ===========================================================================
// kernel sin/cos on [-pi/4, pi/4]  (origin: FreeBSD k_sin.c / k_cos.c)
// ===========================================================================

const KC1: f64 = 4.16666666666666019037e-02;
const KC2: f64 = -1.38888888888741095749e-03;
const KC3: f64 = 2.48015872894767294178e-05;
const KC4: f64 = -2.75573143513906633035e-07;
const KC5: f64 = 2.08757232129817482790e-09;
const KC6: f64 = -1.13596475577881948265e-11;

fn k_cos(x: f64, y: f64) -> f64 {
    let z = x * x;
    let w = z * z;
    let r = z * (KC1 + z * (KC2 + z * KC3)) + w * w * (KC4 + z * (KC5 + z * KC6));
    let hz = 0.5 * z;
    let w = 1.0 - hz;
    w + (((1.0 - w) - hz) + (z * r - x * y))
}

const KS1: f64 = -1.66666666666666324348e-01;
const KS2: f64 = 8.33333333332248946124e-03;
const KS3: f64 = -1.98412698298579493134e-04;
const KS4: f64 = 2.75573137070700676789e-06;
const KS5: f64 = -2.50507602534068634195e-08;
const KS6: f64 = 1.58969099521155010221e-10;

fn k_sin(x: f64, y: f64, iy: i32) -> f64 {
    let z = x * x;
    let w = z * z;
    let r = KS2 + z * (KS3 + z * KS4) + z * w * (KS5 + z * KS6);
    let v = z * x;
    if iy == 0 {
        x + v * (KS1 + z * r)
    } else {
        x - ((z * (0.5 * y - v * r) - y) - v * KS1)
    }
}

// ===========================================================================
// tgamma  (Lanczos approximation, from the libm crate; sinpi via k_sin/k_cos)
// ===========================================================================

const PI: f64 = 3.141592653589793238462643383279502884;

fn sinpi(mut x: f64) -> f64 {
    x = x * 0.5;
    x = 2.0 * (x - x.floor());
    let mut n = (4.0 * x) as isize;
    n = (n + 1) / 2;
    x -= (n as f64) * 0.5;
    x *= PI;
    match n {
        1 => k_cos(x, 0.0),
        2 => k_sin(-x, 0.0, 0),
        3 => -k_cos(x, 0.0),
        _ => k_sin(x, 0.0, 0),
    }
}

const NG: usize = 12;
const GMHALF: f64 = 5.524680040776729583740234375;
const SNUM: [f64; NG + 1] = [
    23531376880.410759688572007674451636754734846804940,
    42919803642.649098768957899047001988850926355848959,
    35711959237.355668049440185451547166705960488635843,
    17921034426.037209699919755754458931112671403265390,
    6039542586.3520280050642916443072979210699388420708,
    1439720407.3117216736632230727949123939715485786772,
    248874557.86205415651146038641322942321632125127801,
    31426415.585400194380614231628318205362874684987640,
    2876370.6289353724412254090516208496135991145378768,
    186056.26539522349504029498971604569928220784236328,
    8071.6720023658162106380029022722506138218516325024,
    210.82427775157934587250973392071336271166969580291,
    2.5066282746310002701649081771338373386264310793408,
];
const SDEN: [f64; NG + 1] = [
    0.0,
    39916800.0,
    120543840.0,
    150917976.0,
    105258076.0,
    45995730.0,
    13339535.0,
    2637558.0,
    357423.0,
    32670.0,
    1925.0,
    66.0,
    1.0,
];
const FACT: [f64; 23] = [
    1.0,
    1.0,
    2.0,
    6.0,
    24.0,
    120.0,
    720.0,
    5040.0,
    40320.0,
    362880.0,
    3628800.0,
    39916800.0,
    479001600.0,
    6227020800.0,
    87178291200.0,
    1307674368000.0,
    20922789888000.0,
    355687428096000.0,
    6402373705728000.0,
    121645100408832000.0,
    2432902008176640000.0,
    51090942171709440000.0,
    1124000727777607680000.0,
];

fn s(x: f64) -> f64 {
    let mut num: f64 = 0.0;
    let mut den: f64 = 0.0;
    if x < 8.0 {
        for i in (0..=NG).rev() {
            num = num * x + SNUM[i];
            den = den * x + SDEN[i];
        }
    } else {
        for i in 0..=NG {
            num = num / x + SNUM[i];
            den = den / x + SDEN[i];
        }
    }
    num / den
}

pub fn tgamma(mut x: f64) -> f64 {
    let u: u64 = x.to_bits();
    let ix: u32 = ((u >> 32) as u32) & 0x7fffffff;
    let sign: bool = (u >> 63) != 0;

    if ix >= 0x7ff00000 {
        return x + f64::INFINITY;
    }
    if ix < ((0x3ff - 54) << 20) {
        return 1.0 / x;
    }
    if x == x.floor() {
        if sign {
            return 0.0 / 0.0;
        }
        if x <= FACT.len() as f64 {
            return FACT[(x as usize) - 1];
        }
    }
    if ix >= 0x40670000 {
        if sign {
            let x1p_126 = f64::from_bits(0x3810000000000000);
            let _ = black_box((x1p_126 / x) as f32);
            if x.floor() * 0.5 == (x * 0.5).floor() {
                return 0.0;
            } else {
                return -0.0;
            }
        }
        let x1p1023 = f64::from_bits(0x7fe0000000000000);
        x *= x1p1023;
        return x;
    }

    let absx = if sign { -x } else { x };
    let mut y = absx + GMHALF;
    let mut dy;
    if absx > GMHALF {
        dy = y - absx;
        dy -= GMHALF;
    } else {
        dy = y - GMHALF;
        dy -= absx;
    }
    let mut z = absx - 0.5;
    let mut r = s(absx) * (-y).exp();
    if x < 0.0 {
        r = -PI / (sinpi(absx) * absx * r);
        dy = -dy;
        z = -z;
    }
    r += dy * (GMHALF + 0.5) * r / y;
    z = y.powf(0.5 * z);
    y = r * z * z;
    y
}

// ===========================================================================
// lgamma  (origin: FreeBSD e_lgamma_r.c)
// ===========================================================================

const LG_PI: f64 = 3.14159265358979311600e+00;
const A0: f64 = 7.72156649015328655494e-02;
const A1: f64 = 3.22467033424113591611e-01;
const A2: f64 = 6.73523010531292681824e-02;
const A3: f64 = 2.05808084325167332806e-02;
const A4: f64 = 7.38555086081402883957e-03;
const A5: f64 = 2.89051383673415629091e-03;
const A6: f64 = 1.19270763183362067845e-03;
const A7: f64 = 5.10069792153511336608e-04;
const A8: f64 = 2.20862790713908385557e-04;
const A9: f64 = 1.08011567247583939954e-04;
const A10: f64 = 2.52144565451257326939e-05;
const A11: f64 = 4.48640949618915160150e-05;
const TC: f64 = 1.46163214496836224576e+00;
const TF: f64 = -1.21486290535849611461e-01;
const TT: f64 = -3.63867699703950536541e-18;
const T0: f64 = 4.83836122723810047042e-01;
const T1: f64 = -1.47587722994593911752e-01;
const T2: f64 = 6.46249402391333854778e-02;
const T3: f64 = -3.27885410759859649565e-02;
const T4: f64 = 1.79706750811820387126e-02;
const T5: f64 = -1.03142241298341437450e-02;
const T6: f64 = 6.10053870246291332635e-03;
const T7: f64 = -3.68452016781138256760e-03;
const T8: f64 = 2.25964780900612472250e-03;
const T9: f64 = -1.40346469989232843813e-03;
const T10: f64 = 8.81081882437654011382e-04;
const T11: f64 = -5.38595305356740546715e-04;
const T12: f64 = 3.15632070903625950361e-04;
const T13: f64 = -3.12754168375120860518e-04;
const T14: f64 = 3.35529192635519073543e-04;
const U0: f64 = -7.72156649015328655494e-02;
const U1: f64 = 6.32827064025093366517e-01;
const U2: f64 = 1.45492250137234768737e+00;
const U3: f64 = 9.77717527963372745603e-01;
const U4: f64 = 2.28963728064692451092e-01;
const U5: f64 = 1.33810918536787660377e-02;
const V1: f64 = 2.45597793713041134822e+00;
const V2: f64 = 2.12848976379893395361e+00;
const V3: f64 = 7.69285150456672783825e-01;
const V4: f64 = 1.04222645593369134254e-01;
const V5: f64 = 3.21709242282423911810e-03;
const S0: f64 = -7.72156649015328655494e-02;
const S1: f64 = 2.14982415960608852501e-01;
const S2: f64 = 3.25778796408930981787e-01;
const S3: f64 = 1.46350472652464452805e-01;
const S4: f64 = 2.66422703033638609560e-02;
const S5: f64 = 1.84028451407337715652e-03;
const S6: f64 = 3.19475326584100867617e-05;
const R1: f64 = 1.39200533467621045958e+00;
const R2: f64 = 7.21935547567138069525e-01;
const R3: f64 = 1.71933865632803078993e-01;
const R4: f64 = 1.86459191715652901344e-02;
const R5: f64 = 7.77942496381893596434e-04;
const R6: f64 = 7.32668430744625636189e-06;
const W0: f64 = 4.18938533204672725052e-01;
const W1: f64 = 8.33333333333329678849e-02;
const W2: f64 = -2.77777777728775536470e-03;
const W3: f64 = 7.93650558643019558500e-04;
const W4: f64 = -5.95187557450339963135e-04;
const W5: f64 = 8.36339918996282139126e-04;
const W6: f64 = -1.63092934096575273989e-03;

fn lg_sin_pi(mut x: f64) -> f64 {
    x = 2.0 * (x * 0.5 - (x * 0.5).floor());
    let mut n = (x * 4.0) as i32;
    n = (n + 1) / 2;
    x -= (n as f64) * 0.5;
    x *= LG_PI;
    match n {
        1 => k_cos(x, 0.0),
        2 => k_sin(-x, 0.0, 0),
        3 => -k_cos(x, 0.0),
        _ => k_sin(x, 0.0, 0),
    }
}

pub fn lgamma(x: f64) -> f64 {
    lgamma_r(x).0
}

fn lgamma_r(mut x: f64) -> (f64, i32) {
    let u: u64 = x.to_bits();
    let mut t: f64;
    let y: f64;
    let mut z: f64;
    let nadj: f64;
    let p: f64;
    let p1: f64;
    let p2: f64;
    let p3: f64;
    let q: f64;
    let mut r: f64;
    let w: f64;
    let ix: u32;
    let sign: bool;
    let i: i32;
    let mut signgam: i32;

    signgam = 1;
    sign = (u >> 63) != 0;
    ix = ((u >> 32) as u32) & 0x7fffffff;
    if ix >= 0x7ff00000 {
        return (x * x, signgam);
    }
    if ix < (0x3ff - 70) << 20 {
        if sign {
            x = -x;
            signgam = -1;
        }
        return (-x.ln(), signgam);
    }
    if sign {
        x = -x;
        t = lg_sin_pi(x);
        if t == 0.0 {
            return (1.0 / (x - x), signgam);
        }
        if t > 0.0 {
            signgam = -1;
        } else {
            t = -t;
        }
        nadj = (LG_PI / (t * x)).ln();
    } else {
        nadj = 0.0;
    }

    if (ix == 0x3ff00000 || ix == 0x40000000) && (u & 0xffffffff) == 0 {
        r = 0.0;
    } else if ix < 0x40000000 {
        if ix <= 0x3feccccc {
            r = -x.ln();
            if ix >= 0x3FE76944 {
                y = 1.0 - x;
                i = 0;
            } else if ix >= 0x3FCDA661 {
                y = x - (TC - 1.0);
                i = 1;
            } else {
                y = x;
                i = 2;
            }
        } else {
            r = 0.0;
            if ix >= 0x3FFBB4C3 {
                y = 2.0 - x;
                i = 0;
            } else if ix >= 0x3FF3B4C4 {
                y = x - TC;
                i = 1;
            } else {
                y = x - 1.0;
                i = 2;
            }
        }
        match i {
            0 => {
                z = y * y;
                p1 = A0 + z * (A2 + z * (A4 + z * (A6 + z * (A8 + z * A10))));
                p2 = z * (A1 + z * (A3 + z * (A5 + z * (A7 + z * (A9 + z * A11)))));
                p = y * p1 + p2;
                r += p - 0.5 * y;
            }
            1 => {
                z = y * y;
                w = z * y;
                p1 = T0 + w * (T3 + w * (T6 + w * (T9 + w * T12)));
                p2 = T1 + w * (T4 + w * (T7 + w * (T10 + w * T13)));
                p3 = T2 + w * (T5 + w * (T8 + w * (T11 + w * T14)));
                p = z * p1 - (TT - w * (p2 + y * p3));
                r += TF + p;
            }
            2 => {
                p1 = y * (U0 + y * (U1 + y * (U2 + y * (U3 + y * (U4 + y * U5)))));
                p2 = 1.0 + y * (V1 + y * (V2 + y * (V3 + y * (V4 + y * V5))));
                r += -0.5 * y + p1 / p2;
            }
            _ => {}
        }
    } else if ix < 0x40200000 {
        i = x as i32;
        y = x - (i as f64);
        p = y * (S0 + y * (S1 + y * (S2 + y * (S3 + y * (S4 + y * (S5 + y * S6))))));
        q = 1.0 + y * (R1 + y * (R2 + y * (R3 + y * (R4 + y * (R5 + y * R6)))));
        r = 0.5 * y + p / q;
        z = 1.0;
        if i >= 7 {
            z *= y + 6.0;
        }
        if i >= 6 {
            z *= y + 5.0;
        }
        if i >= 5 {
            z *= y + 4.0;
        }
        if i >= 4 {
            z *= y + 3.0;
        }
        if i >= 3 {
            z *= y + 2.0;
            r += z.ln();
        }
    } else if ix < 0x43900000 {
        t = x.ln();
        z = 1.0 / x;
        y = z * z;
        w = W0 + z * (W1 + y * (W2 + y * (W3 + y * (W4 + y * (W5 + y * W6)))));
        r = (x - 0.5) * (t - 1.0) + w;
    } else {
        r = x * (x.ln() - 1.0);
    }
    if sign {
        r = nadj - r;
    }
    (r, signgam)
}
