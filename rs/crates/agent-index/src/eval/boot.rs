//! The paired bootstrap, with Python's own random numbers (MT19937 as
//! CPython seeds and draws it), so an interval here is the one the Python
//! harness printed for the same data and seed, digit for digit.

/// CPython's `random.Random(seed)` for a non-negative integer seed.
pub struct PyRandom {
    mt: [u32; 624],
    i: usize,
}

impl PyRandom {
    pub fn new(seed: u64) -> PyRandom {
        // init_by_array over the seed's 32-bit words, least significant first.
        let mut key = vec![seed as u32];
        if seed >> 32 != 0 {
            key.push((seed >> 32) as u32);
        }
        let mut r = PyRandom {
            mt: [0; 624],
            i: 624,
        };
        r.init_genrand(19_650_218);
        let n = 624usize;
        let (mut i, mut j) = (1usize, 0usize);
        for _ in 0..n.max(key.len()) {
            let prev = r.mt[i - 1] ^ (r.mt[i - 1] >> 30);
            r.mt[i] = (r.mt[i] ^ prev.wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= n {
                r.mt[0] = r.mt[n - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..n - 1 {
            let prev = r.mt[i - 1] ^ (r.mt[i - 1] >> 30);
            r.mt[i] = (r.mt[i] ^ prev.wrapping_mul(1_566_083_941)).wrapping_sub(i as u32);
            i += 1;
            if i >= n {
                r.mt[0] = r.mt[n - 1];
                i = 1;
            }
        }
        r.mt[0] = 0x8000_0000;
        r
    }

    fn init_genrand(&mut self, s: u32) {
        self.mt[0] = s;
        for i in 1..624 {
            let prev = self.mt[i - 1] ^ (self.mt[i - 1] >> 30);
            self.mt[i] = 1_812_433_253u32.wrapping_mul(prev).wrapping_add(i as u32);
        }
        self.i = 624;
    }

    fn next_u32(&mut self) -> u32 {
        if self.i >= 624 {
            for k in 0..624 {
                let y = (self.mt[k] & 0x8000_0000) | (self.mt[(k + 1) % 624] & 0x7fff_ffff);
                let mut v = self.mt[(k + 397) % 624] ^ (y >> 1);
                if y & 1 != 0 {
                    v ^= 0x9908_b0df;
                }
                self.mt[k] = v;
            }
            self.i = 0;
        }
        let mut y = self.mt[self.i];
        self.i += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    /// `random.random()`: 53 bits from two draws.
    pub fn random(&mut self) -> f64 {
        let a = (self.next_u32() >> 5) as f64;
        let b = (self.next_u32() >> 6) as f64;
        (a * 67_108_864.0 + b) * (1.0 / 9_007_199_254_740_992.0)
    }

    /// `random.getrandbits(k)` for k up to 32.
    pub fn getrandbits(&mut self, k: u32) -> u32 {
        debug_assert!((1..=32).contains(&k));
        self.next_u32() >> (32 - k)
    }

    /// The index `random.choice` picks among n (`_randbelow`, by rejection).
    pub fn choice(&mut self, n: usize) -> usize {
        let k = usize::BITS - n.leading_zeros();
        loop {
            let r = self.getrandbits(k) as usize;
            if r < n {
                return r;
            }
        }
    }

    /// The index each draw of `random.choices(population, k=...)` picks
    /// among n (no weights: `floor(random() * n)`).
    pub fn choices_index(&mut self, n: usize) -> usize {
        (self.random() * n as f64).floor() as usize
    }
}

/// The 2.5% and 97.5% points of sorted resamples, as the scripts index them.
pub fn interval(sorted: &[f64]) -> (f64, f64) {
    let r = sorted.len();
    if r == 0 {
        return (0.0, 0.0);
    }
    let lo = (0.025 * r as f64) as usize;
    let hi = ((0.975 * r as f64) as usize).min(r - 1);
    (sorted[lo], sorted[hi])
}

/// One resampling unit (a question, or a session's queries together): its
/// summed differences per measure and its query count.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    pub sums: Vec<f64>,
    pub n: usize,
}

/// run_eval.py's paired bootstrap: whole units drawn with `choices`, each
/// measure's mean difference over the drawn queries. Per measure: (the
/// point difference, the interval).
pub fn units(cells: &[Cell], measures: usize, resamples: usize, seed: u64) -> Vec<(f64, f64, f64)> {
    let n: usize = cells.iter().map(|c| c.n).sum();
    let mut draws: Vec<Vec<f64>> = vec![Vec::with_capacity(resamples); measures];
    let mut rng = PyRandom::new(seed);
    for _ in 0..resamples {
        let mut size = 0usize;
        let mut sums = vec![0.0; measures];
        // Python draws every index of the sample first, then sums: the same
        // sequence of draws either way.
        let pick: Vec<usize> = (0..cells.len())
            .map(|_| rng.choices_index(cells.len()))
            .collect();
        for &p in &pick {
            size += cells[p].n;
            for (m, s) in sums.iter_mut().enumerate() {
                *s += cells[p].sums[m];
            }
        }
        for (m, s) in sums.into_iter().enumerate() {
            draws[m].push(s / size as f64);
        }
    }
    (0..measures)
        .map(|m| {
            let point = cells.iter().map(|c| c.sums[m]).sum::<f64>() / n as f64;
            let mut d = std::mem::take(&mut draws[m]);
            d.sort_by(|a, b| a.total_cmp(b));
            let (lo, hi) = interval(&d);
            (point, lo, hi)
        })
        .collect()
}

/// blind3_score.py's paired bootstrap: per-question differences drawn with
/// `choices`, k = n. (The point difference, the interval.)
pub fn paired(diffs: &[f64], resamples: usize, seed: u64) -> (f64, f64, f64) {
    let n = diffs.len();
    if n == 0 {
        return (0.0, 0.0, 0.0);
    }
    let mut rng = PyRandom::new(seed);
    let mut d: Vec<f64> = (0..resamples)
        .map(|_| {
            let mut s = 0.0;
            for _ in 0..n {
                s += diffs[rng.choices_index(n)];
            }
            s / n as f64
        })
        .collect();
    d.sort_by(|a, b| a.total_cmp(b));
    let (lo, hi) = interval(&d);
    (diffs.iter().sum::<f64>() / n as f64, lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values CPython 3 gives for these seeds and calls.
    #[test]
    fn matches_cpython() {
        let mut r = PyRandom::new(20_261_123);
        let got: Vec<f64> = (0..3).map(|_| r.random()).collect();
        assert_eq!(
            got,
            [0.7548469049910241, 0.8974816498515421, 0.6549820415218462]
        );
        let mut r = PyRandom::new(20_261_123);
        let got: Vec<usize> = (0..12).map(|_| r.choices_index(7)).collect();
        assert_eq!(got, [5, 6, 4, 1, 5, 0, 5, 2, 1, 4, 0, 2]);
        let mut r = PyRandom::new(20_261_123);
        let got: Vec<usize> = (0..12).map(|_| r.choice(60)).collect();
        assert_eq!(got, [48, 36, 57, 41, 31, 10, 11, 52, 35, 5, 34, 54]);
        let mut r = PyRandom::new(7);
        assert_eq!(r.getrandbits(32), 1_390_851_128);
        assert_eq!(r.random(), 0.9478653606090632);
        assert_eq!(PyRandom::new((1 << 40) + 5).random(), 0.5043802970418443);
    }

    /// The intervals blind3_score.py's and run_eval.py's bootstraps give
    /// for the same data, seed and resamples.
    #[test]
    fn intervals_as_the_scripts() {
        let d = [1.0, 0.0, 0.0, 1.0, -1.0, 0.0, 1.0, 1.0, 0.0, 0.0];
        assert_eq!(paired(&d, 1000, 20_261_123), (0.3, -0.1, 0.7));
        let cells = [
            (1.0, 0.5, 0.2, 2),
            (0.0, -0.25, 0.0, 1),
            (-1.0, -1.0, -0.1, 1),
            (1.0, 1.0, 0.3, 3),
        ]
        .map(|(a, b, c, n)| Cell {
            sums: vec![a, b, c],
            n,
        });
        let got = units(&cells, 3, 1000, 20_261_123);
        assert_eq!((got[0].1, got[0].2), (-0.5, 0.42857142857142855));
        assert_eq!((got[1].1, got[1].2), (-0.625, 0.3));
        assert_eq!((got[2].1, got[2].2), (-0.05, 0.1));
    }
}
