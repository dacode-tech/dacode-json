//! Deterministic JSON corpus generation for tests and benchmarks.
//!
//! Everything here is reproducible from a seed so benchmark numbers can be
//! compared across runs and machines without shipping data files.
//!
//! The shapes mirror the workloads Vela measures in
//! `bootstrap/tests/velac2/t860_json_fair_bench.vl` and `t861_json_bench_10mb.vl`.

/// A tiny xorshift PRNG — reproducible and dependency-free.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Rng(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    #[inline]
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    #[inline]
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(self.below(items.len() as u64) as usize)
    }
}

const WORDS: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango",
];

/// An array of flat objects — the classic "records" workload. Integer-only,
/// so the faithful and strict parsers agree on every value.
///
/// Roughly 110 bytes per record.
#[must_use]
pub fn records(count: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut s = String::with_capacity(count * 120 + 2);
    s.push('[');
    for i in 0..count {
        if i > 0 {
            s.push(',');
        }
        let name = rng.pick(WORDS).copied().unwrap_or("x");
        let city = rng.pick(WORDS).copied().unwrap_or("y");
        s.push_str(r#"{"id":"#);
        push_u64(&mut s, i as u64);
        s.push_str(r#","name":""#);
        s.push_str(name);
        s.push_str(r#"","age":"#);
        push_u64(&mut s, 18 + rng.below(60));
        s.push_str(r#","active":"#);
        s.push_str(if rng.below(2) == 0 { "false" } else { "true" });
        s.push_str(r#","city":""#);
        s.push_str(city);
        s.push_str(r#"","score":"#);
        push_u64(&mut s, rng.below(100_000));
        s.push_str(r#","tags":["#);
        for t in 0..rng.below(4) {
            if t > 0 {
                s.push(',');
            }
            s.push('"');
            s.push_str(rng.pick(WORDS).copied().unwrap_or("z"));
            s.push('"');
        }
        s.push_str("]}");
    }
    s.push(']');
    s
}

/// Deeply nested objects — stresses the container stack and `skip_subtree`.
///
/// `depth` is clamped to just under [`crate::pool::STACK_MAX`] so the
/// faithful parser does not silently truncate.
#[must_use]
pub fn nested(depth: usize, seed: u64) -> String {
    let depth = depth.min(crate::pool::STACK_MAX - 2);
    let mut rng = Rng::new(seed);
    let mut s = String::new();
    for i in 0..depth {
        s.push_str(r#"{"k"#);
        push_u64(&mut s, i as u64);
        s.push_str(r#"":"#);
    }
    push_u64(&mut s, rng.below(1000));
    for _ in 0..depth {
        s.push('}');
    }
    s
}

/// A large flat array of integers — the pathological node-density case
/// Vela's `len / 2 + 64` pool heuristic is sized for.
#[must_use]
pub fn int_array(count: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut s = String::with_capacity(count * 6 + 2);
    s.push('[');
    for i in 0..count {
        if i > 0 {
            s.push(',');
        }
        push_u64(&mut s, rng.below(10_000));
    }
    s.push(']');
    s
}

/// String-heavy data with escapes — exercises the escape-detection path in
/// Stage 1 that the branchless scanner exists for.
#[must_use]
pub fn strings(count: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut s = String::with_capacity(count * 40 + 2);
    s.push('[');
    for i in 0..count {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        for _ in 0..(4 + rng.below(8)) {
            s.push_str(rng.pick(WORDS).copied().unwrap_or("w"));
            match rng.below(8) {
                0 => s.push_str(r#"\""#),
                1 => s.push_str(r"\\"),
                2 => s.push_str(r"\n"),
                3 => s.push_str(r"\u00e9"),
                _ => s.push(' '),
            }
        }
        s.push('"');
    }
    s.push(']');
    s
}

/// A GeoJSON-ish document: nested arrays of coordinate pairs.
///
/// Note the coordinates are **integers**. Using real floats here would make
/// the faithful parser produce nonsense (`3.14` -> `314`) and the comparison
/// against serde_json meaningless; see `docs/RESULTS.md`.
#[must_use]
pub fn geo_int(features: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut s = String::new();
    s.push_str(r#"{"type":"FeatureCollection","features":["#);
    for f in 0..features {
        if f > 0 {
            s.push(',');
        }
        s.push_str(r#"{"type":"Feature","properties":{"name":""#);
        s.push_str(rng.pick(WORDS).copied().unwrap_or("p"));
        s.push_str(r#"","rank":"#);
        push_u64(&mut s, rng.below(100));
        s.push_str(r#"},"geometry":{"type":"Polygon","coordinates":[["#);
        for c in 0..(8 + rng.below(24)) {
            if c > 0 {
                s.push(',');
            }
            s.push('[');
            push_u64(&mut s, rng.below(360));
            s.push(',');
            push_u64(&mut s, rng.below(180));
            s.push(']');
        }
        s.push_str("]]}}");
    }
    s.push_str("]}");
    s
}

/// The same shape as [`geo_int`] but with real decimal coordinates.
///
/// Only meaningful for the strict parser and the reference libraries.
#[must_use]
pub fn geo_float(features: usize, seed: u64) -> String {
    let mut rng = Rng::new(seed);
    let mut s = String::new();
    s.push_str(r#"{"type":"FeatureCollection","features":["#);
    for f in 0..features {
        if f > 0 {
            s.push(',');
        }
        s.push_str(r#"{"type":"Feature","properties":{"name":""#);
        s.push_str(rng.pick(WORDS).copied().unwrap_or("p"));
        s.push_str(r#"","rank":"#);
        push_u64(&mut s, rng.below(100));
        s.push_str(r#"},"geometry":{"type":"Polygon","coordinates":[["#);
        for c in 0..(8 + rng.below(24)) {
            if c > 0 {
                s.push(',');
            }
            s.push('[');
            push_fixed(&mut s, rng.below(360_000_000), 6);
            s.push(',');
            push_fixed(&mut s, rng.below(180_000_000), 6);
            s.push(']');
        }
        s.push_str("]]}}");
    }
    s.push_str("]}");
    s
}

/// A random but **valid** JSON document, for differential fuzzing.
///
/// Covers every value type, empty containers, escapes (including `\u` and
/// surrogate pairs), negative numbers and floats.
#[must_use]
pub fn random_value(rng: &mut Rng, max_depth: u32) -> String {
    let mut s = String::new();
    write_value(&mut s, rng, max_depth);
    s
}

fn write_value(s: &mut String, rng: &mut Rng, depth: u32) {
    // At depth 0 only scalars, so generation always terminates.
    let choice = if depth == 0 { rng.below(6) } else { rng.below(8) };
    match choice {
        0 => s.push_str("null"),
        1 => s.push_str(if rng.below(2) == 0 { "true" } else { "false" }),
        2 => {
            if rng.below(2) == 0 {
                s.push('-');
            }
            push_u64(s, rng.below(1_000_000));
        }
        3 => {
            if rng.below(2) == 0 {
                s.push('-');
            }
            push_fixed(s, rng.below(1_000_000_000), 4);
            if rng.below(3) == 0 {
                s.push('e');
                s.push(if rng.below(2) == 0 { '+' } else { '-' });
                push_u64(s, rng.below(20));
            }
        }
        4 | 5 => write_string(s, rng),
        6 => {
            s.push('[');
            let n = rng.below(5);
            for i in 0..n {
                if i > 0 {
                    s.push(',');
                }
                write_value(s, rng, depth - 1);
            }
            s.push(']');
        }
        _ => {
            s.push('{');
            let n = rng.below(5);
            for i in 0..n {
                if i > 0 {
                    s.push(',');
                }
                // Keys must be unique: `serde_json::Map` silently keeps only
                // the last of a duplicated key, which would make any
                // structural comparison against it meaningless.
                write_unique_key(s, rng, i);
                s.push(':');
                write_value(s, rng, depth - 1);
            }
            s.push('}');
        }
    }
}

fn write_unique_key(s: &mut String, rng: &mut Rng, ordinal: u64) {
    s.push('"');
    push_u64(s, ordinal);
    s.push('_');
    for _ in 0..rng.below(6) {
        match rng.below(8) {
            0 => s.push_str(r#"\""#),
            1 => s.push_str(r"\\"),
            2 => s.push_str(r"\n"),
            3 => s.push_str(r"\u00e9"),
            4 => s.push_str(r"\ud83d\ude00"),
            5 => s.push('é'),
            6 => s.push_str("{}[],:"),
            _ => s.push_str(rng.pick(WORDS).copied().unwrap_or("q")),
        }
    }
    s.push('"');
}

fn write_string(s: &mut String, rng: &mut Rng) {
    s.push('"');
    for _ in 0..rng.below(10) {
        match rng.below(12) {
            0 => s.push_str(r#"\""#),
            1 => s.push_str(r"\\"),
            2 => s.push_str(r"\n"),
            3 => s.push_str(r"\t"),
            4 => s.push_str(r"\/"),
            5 => s.push_str(r"\u00e9"),
            6 => s.push_str(r"\ud83d\ude00"),
            7 => s.push('é'),
            8 => s.push(' '),
            9 => s.push_str("{}[],:"),
            _ => s.push_str(rng.pick(WORDS).copied().unwrap_or("q")),
        }
    }
    s.push('"');
}

/// Grow `make(n)` until the output is at least `target_bytes`.
#[must_use]
pub fn sized(target_bytes: usize, seed: u64, make: fn(usize, u64) -> String) -> String {
    let mut n = 64usize;
    loop {
        let s = make(n, seed);
        if s.len() >= target_bytes || n > 40_000_000 {
            return s;
        }
        // Extrapolate, with a floor so we always make progress.
        let per = (s.len() / n).max(1);
        n = ((target_bytes / per) + 1).max(n * 2);
    }
}

fn push_u64(s: &mut String, mut v: u64) {
    if v == 0 {
        s.push('0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        if let Some(slot) = buf.get_mut(i) {
            *slot = b'0' + (v % 10) as u8;
        }
        v /= 10;
    }
    if let Some(digits) = buf.get(i..) {
        s.push_str(&String::from_utf8_lossy(digits));
    }
}

fn push_fixed(s: &mut String, v: u64, decimals: u32) {
    let div = 10u64.pow(decimals);
    push_u64(s, v / div);
    s.push('.');
    let mut frac = v % div;
    let mut scale = div / 10;
    while scale > 0 {
        s.push((b'0' + (frac / scale) as u8) as char);
        frac %= scale;
        scale /= 10;
    }
}

/// Every named corpus, as `(name, json)`.
#[must_use]
pub fn suite(target_bytes: usize, seed: u64) -> Vec<(&'static str, String)> {
    vec![
        ("records", sized(target_bytes, seed, records)),
        ("int_array", sized(target_bytes, seed, int_array)),
        ("strings", sized(target_bytes, seed, strings)),
        ("geo_int", sized(target_bytes, seed, geo_int)),
        ("geo_float", sized(target_bytes, seed, geo_float)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators_produce_valid_json() {
        for (name, json) in suite(20_000, 7) {
            serde_json::from_str::<serde_json::Value>(&json)
                .unwrap_or_else(|e| panic!("{name} is invalid JSON: {e}"));
            assert!(json.len() >= 20_000, "{name} too small: {}", json.len());
        }
        serde_json::from_str::<serde_json::Value>(&nested(100, 1)).expect("nested is valid");
    }

    #[test]
    fn deterministic() {
        assert_eq!(records(50, 3), records(50, 3));
        assert_ne!(records(50, 3), records(50, 4));
    }
}
