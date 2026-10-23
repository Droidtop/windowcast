//! Reed-Solomon erasure coding over GF(2^8), the polynomial 0x11d.
//!
//! A [`Code`] turns `data` equal-sized shards into `parity` more, and any
//! `data` of the resulting shards (data or parity, in any mix) give the data
//! shards back. It is used two ways:
//!
//! - GameStream audio uses a fixed 4+2 code with the exact parity matrix
//!   Sunshine and moonlight-common-c install ([`Code::new`] with that matrix),
//!   so stock Moonlight can read it. That matrix lives in `windowcast-gamestream`.
//! - Direct video picks a ratio per frame from the loss it measures
//!   ([`Code::cauchy`]): more parity when the network drops more.
//!
//! A shard's *index* says what it is: indices below `data` are data shards, the
//! rest are parity shard `index - data`. [`Code::recover`] takes the shards in
//! index order with `None` for each one that did not arrive.

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CodeError {
    #[error(
        "a code needs at least one data shard and a matrix row of that length per parity shard"
    )]
    Shape,
    #[error(
        "{data} data and {parity} parity shards do not fit in GF(2^8), which has 256 elements"
    )]
    TooManyShards { data: usize, parity: usize },
    #[error("expected {expected} shards, got {got}")]
    ShardCount { expected: usize, got: usize },
    #[error("the shards are not all the same size")]
    ShardSize,
}

struct Tables {
    log: [u8; 256],
    exp: [u8; 512],
}

const TABLES: Tables = {
    let mut log = [0u8; 256];
    let mut exp = [0u8; 512];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        exp[i + 255] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11d;
        }
        i += 1;
    }
    Tables { log, exp }
};

/// Multiplication in the field.
pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    TABLES.exp[usize::from(TABLES.log[usize::from(a)]) + usize::from(TABLES.log[usize::from(b)])]
}

/// The multiplicative inverse. `a` must not be 0.
pub fn inverse(a: u8) -> u8 {
    debug_assert!(a != 0, "0 has no inverse");
    TABLES.exp[255 - usize::from(TABLES.log[usize::from(a)])]
}

/// `out ^= coefficient * shard`, byte by byte.
fn add_scaled(out: &mut [u8], coefficient: u8, shard: &[u8]) {
    if coefficient == 0 {
        return;
    }
    for (o, b) in out.iter_mut().zip(shard) {
        *o ^= mul(coefficient, *b);
    }
}

/// An erasure code: the matrix that makes each parity shard from the data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    data: usize,
    /// `parity[j][i]` times data shard `i`, summed over `i`, is parity shard `j`.
    parity: Vec<Vec<u8>>,
}

impl Code {
    /// A code from an explicit parity matrix, one row per parity shard and one
    /// column per data shard. The caller vouches that any `data` shards of the
    /// result can recover the data; [`Code::recover`] returns `None` for a
    /// combination that cannot.
    pub fn new(data: usize, parity: Vec<Vec<u8>>) -> Result<Code, CodeError> {
        if data == 0 || parity.iter().any(|row| row.len() != data) {
            return Err(CodeError::Shape);
        }
        Ok(Code { data, parity })
    }

    /// A code with `data` data shards and `parity` parity shards from a Cauchy
    /// matrix, whose every square sub-matrix is invertible: any `data` of the
    /// `data + parity` shards recover the data, with nothing to check.
    pub fn cauchy(data: usize, parity: usize) -> Result<Code, CodeError> {
        if data == 0 {
            return Err(CodeError::Shape);
        }
        if data + parity > 256 {
            return Err(CodeError::TooManyShards { data, parity });
        }
        // Row element x_j = data + j and column element y_i = i are all
        // different, so x_j + y_i (addition is XOR) is never 0.
        let rows = (0..parity)
            .map(|j| {
                (0..data)
                    .map(|i| inverse(((data + j) ^ i) as u8))
                    .collect::<Vec<u8>>()
            })
            .collect();
        Ok(Code { data, parity: rows })
    }

    pub fn data_shards(&self) -> usize {
        self.data
    }

    pub fn parity_shards(&self) -> usize {
        self.parity.len()
    }

    pub fn shards(&self) -> usize {
        self.data + self.parity.len()
    }

    /// The coefficients that make shard `index` from the data shards: a unit
    /// row for a data shard, the matrix row for a parity shard.
    fn row(&self, index: usize) -> Vec<u8> {
        if index < self.data {
            let mut unit = vec![0; self.data];
            unit[index] = 1;
            unit
        } else {
            self.parity[index - self.data].clone()
        }
    }

    /// The parity shards for `data`, which must be `data_shards()` shards of
    /// one size.
    pub fn encode(&self, data: &[&[u8]]) -> Result<Vec<Vec<u8>>, CodeError> {
        if data.len() != self.data {
            return Err(CodeError::ShardCount {
                expected: self.data,
                got: data.len(),
            });
        }
        let size = data[0].len();
        if data.iter().any(|shard| shard.len() != size) {
            return Err(CodeError::ShardSize);
        }
        Ok(self
            .parity
            .iter()
            .map(|row| {
                let mut out = vec![0u8; size];
                for (coefficient, shard) in row.iter().zip(data) {
                    add_scaled(&mut out, *coefficient, shard);
                }
                out
            })
            .collect())
    }

    /// The data shards from any `data_shards()` of the `shards()` shards (all
    /// one size). `shards` is in index order with `None` for a missing one.
    /// `None` when too few arrived, they differ in size, or (for a hand-made
    /// matrix) the ones that did arrive cannot recover the data.
    pub fn recover(&self, shards: &[Option<Vec<u8>>]) -> Option<Vec<Vec<u8>>> {
        if shards.len() != self.shards() {
            return None;
        }
        if shards[..self.data].iter().all(Option::is_some) {
            return Some(shards[..self.data].iter().flatten().cloned().collect());
        }
        let present: Vec<usize> = (0..shards.len())
            .filter(|i| shards[*i].is_some())
            .take(self.data)
            .collect();
        if present.len() < self.data {
            return None;
        }
        let size = shards[present[0]].as_ref()?.len();
        if present
            .iter()
            .any(|i| shards[*i].as_ref().map(Vec::len) != Some(size))
        {
            return None;
        }
        // Invert the matrix of the shards that came (Gauss-Jordan): row `r`
        // says how shard `present[r]` was made from the data.
        let n = self.data;
        let mut m: Vec<Vec<u8>> = present.iter().map(|i| self.row(*i)).collect();
        let mut inv: Vec<Vec<u8>> = (0..n)
            .map(|r| (0..n).map(|c| u8::from(r == c)).collect())
            .collect();
        for col in 0..n {
            let pivot = (col..n).find(|r| m[*r][col] != 0)?;
            m.swap(col, pivot);
            inv.swap(col, pivot);
            let scale = inverse(m[col][col]);
            for c in 0..n {
                m[col][c] = mul(m[col][c], scale);
                inv[col][c] = mul(inv[col][c], scale);
            }
            for r in 0..n {
                let factor = m[r][col];
                if r != col && factor != 0 {
                    for c in 0..n {
                        let (a, b) = (mul(factor, m[col][c]), mul(factor, inv[col][c]));
                        m[r][c] ^= a;
                        inv[r][c] ^= b;
                    }
                }
            }
        }
        Some(
            (0..n)
                .map(|d| {
                    if let Some(shard) = &shards[d] {
                        return shard.clone();
                    }
                    let mut out = vec![0u8; size];
                    for (k, index) in present.iter().enumerate() {
                        let shard = shards[*index].as_ref().expect("present");
                        add_scaled(&mut out, inv[d][k], shard);
                    }
                    out
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_shards(count: usize, size: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| {
                (0..size)
                    .map(|b| (b as u8).wrapping_mul(31).wrapping_add((i * 7) as u8))
                    .collect()
            })
            .collect()
    }

    fn all_shards(code: &Code, data: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let refs: Vec<&[u8]> = data.iter().map(Vec::as_slice).collect();
        let parity = code.encode(&refs).unwrap();
        data.iter().chain(parity.iter()).cloned().collect()
    }

    /// Every way of losing up to `parity` shards, for every size of code up to
    /// a few shards: data comes back each time.
    #[test]
    fn a_cauchy_code_recovers_from_any_loss_up_to_its_parity() {
        for data in 1..=5usize {
            for parity in 0..=4usize {
                let code = Code::cauchy(data, parity).unwrap();
                let original = data_shards(data, 29);
                let all = all_shards(&code, &original);
                let n = code.shards();
                for mask in 0u32..(1 << n) {
                    let lost = mask.count_ones() as usize;
                    let shards: Vec<Option<Vec<u8>>> = (0..n)
                        .map(|i| (mask >> i & 1 == 0).then(|| all[i].clone()))
                        .collect();
                    let back = code.recover(&shards);
                    if lost <= parity {
                        assert_eq!(
                            back,
                            Some(original.clone()),
                            "{data}+{parity} lost {mask:b}"
                        );
                    } else {
                        assert_eq!(back, None, "{data}+{parity} lost {mask:b}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_big_code_recovers_a_scattered_loss() {
        // 100 data and 20 parity: lose 20 shards spread over both kinds.
        let code = Code::cauchy(100, 20).unwrap();
        let original = data_shards(100, 64);
        let all = all_shards(&code, &original);
        let shards: Vec<Option<Vec<u8>>> = (0..120)
            .map(|i| (i % 6 != 0).then(|| all[i].clone()))
            .collect();
        assert_eq!(shards.iter().filter(|s| s.is_none()).count(), 20);
        assert_eq!(code.recover(&shards), Some(original));
    }

    #[test]
    fn the_shape_of_a_code_is_checked() {
        assert_eq!(Code::cauchy(0, 2), Err(CodeError::Shape));
        assert_eq!(
            Code::cauchy(200, 57),
            Err(CodeError::TooManyShards {
                data: 200,
                parity: 57
            })
        );
        assert!(Code::cauchy(200, 56).is_ok());
        assert_eq!(Code::new(2, vec![vec![1]]), Err(CodeError::Shape));
    }

    #[test]
    fn encode_refuses_the_wrong_number_or_size_of_shards() {
        let code = Code::cauchy(3, 1).unwrap();
        assert_eq!(
            code.encode(&[b"ab".as_slice(), b"cd".as_slice()]),
            Err(CodeError::ShardCount {
                expected: 3,
                got: 2
            })
        );
        assert_eq!(
            code.encode(&[b"ab".as_slice(), b"cd".as_slice(), b"e".as_slice()]),
            Err(CodeError::ShardSize)
        );
    }

    #[test]
    fn recover_refuses_shards_of_different_sizes_or_the_wrong_count() {
        let code = Code::cauchy(2, 1).unwrap();
        let ok = Some(vec![1u8, 2]);
        assert_eq!(code.recover(&[None, ok.clone()]), None);
        assert_eq!(code.recover(&[None, Some(vec![1, 2, 3]), ok.clone()]), None);
    }

    #[test]
    fn the_field_has_unit_inverses_and_the_expected_log_table() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inverse(a)), 1);
        }
        // Polynomial 285: log 2 = 1, log 3 = 25, log 4 = 2.
        assert_eq!(TABLES.log[2], 1);
        assert_eq!(TABLES.log[3], 25);
        assert_eq!(TABLES.log[4], 2);
    }
}
