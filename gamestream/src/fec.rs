//! Reed-Solomon parity for GameStream audio: four data packets, two parity
//! packets, over GF(2^8) with the polynomial 0x11d. The parity matrix is
//! the fixed one Sunshine and moonlight-common-c both install (the matrix
//! NVIDIA's hosts used, taken from OpenFEC): parity `j` is the sum over the
//! data shards `i` of `PARITY[j][i]` times shard `i`.

pub const DATA_SHARDS: usize = 4;
pub const PARITY_SHARDS: usize = 2;
pub const SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;

const PARITY: [[u8; DATA_SHARDS]; PARITY_SHARDS] =
    [[0x77, 0x40, 0x38, 0x0e], [0xc7, 0xa7, 0x0d, 0x6c]];

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

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    TABLES.exp[usize::from(TABLES.log[usize::from(a)]) + usize::from(TABLES.log[usize::from(b)])]
}

fn inverse(a: u8) -> u8 {
    TABLES.exp[255 - usize::from(TABLES.log[usize::from(a)])]
}

/// The coefficients that make shard `index` from the data shards: a unit
/// row for a data shard, the parity row for a parity shard.
fn row(index: usize) -> [u8; DATA_SHARDS] {
    if index < DATA_SHARDS {
        let mut unit = [0; DATA_SHARDS];
        unit[index] = 1;
        unit
    } else {
        PARITY[index - DATA_SHARDS]
    }
}

/// The two parity shards for four data shards of one size.
pub fn parity(data: [&[u8]; DATA_SHARDS]) -> [Vec<u8>; PARITY_SHARDS] {
    let size = data[0].len();
    std::array::from_fn(|j| {
        let mut out = vec![0u8; size];
        for (i, shard) in data.iter().enumerate() {
            let c = PARITY[j][i];
            for (o, b) in out.iter_mut().zip(shard.iter()) {
                *o ^= mul(c, *b);
            }
        }
        out
    })
}

/// The four data shards from any four of the six (all one size); `None`
/// when fewer than four arrived.
pub fn recover(shards: &[Option<Vec<u8>>; SHARDS]) -> Option<[Vec<u8>; DATA_SHARDS]> {
    if shards[..DATA_SHARDS].iter().all(Option::is_some) {
        return Some(std::array::from_fn(|i| shards[i].clone().unwrap()));
    }
    let present: Vec<usize> = (0..SHARDS)
        .filter(|i| shards[*i].is_some())
        .take(DATA_SHARDS)
        .collect();
    if present.len() < DATA_SHARDS {
        return None;
    }
    let size = shards[present[0]].as_ref()?.len();
    if present
        .iter()
        .any(|i| shards[*i].as_ref().map(Vec::len) != Some(size))
    {
        return None;
    }
    // Invert the 4x4 matrix of the shards that came (Gauss-Jordan).
    let mut m: [[u8; DATA_SHARDS]; DATA_SHARDS] = std::array::from_fn(|r| row(present[r]));
    let mut inv: [[u8; DATA_SHARDS]; DATA_SHARDS] =
        std::array::from_fn(|r| std::array::from_fn(|c| u8::from(r == c)));
    for col in 0..DATA_SHARDS {
        let pivot = (col..DATA_SHARDS).find(|r| m[*r][col] != 0)?;
        m.swap(col, pivot);
        inv.swap(col, pivot);
        let scale = inverse(m[col][col]);
        for c in 0..DATA_SHARDS {
            m[col][c] = mul(m[col][c], scale);
            inv[col][c] = mul(inv[col][c], scale);
        }
        for r in 0..DATA_SHARDS {
            let factor = m[r][col];
            if r != col && factor != 0 {
                for c in 0..DATA_SHARDS {
                    m[r][c] ^= mul(factor, m[col][c]);
                    inv[r][c] ^= mul(factor, inv[col][c]);
                }
            }
        }
    }
    Some(std::array::from_fn(|d| {
        if let Some(shard) = &shards[d] {
            return shard.clone();
        }
        let mut out = vec![0u8; size];
        for (k, index) in present.iter().enumerate() {
            let c = inv[d][k];
            let shard = shards[*index].as_ref().expect("present");
            for (o, b) in out.iter_mut().zip(shard) {
                *o ^= mul(c, *b);
            }
        }
        out
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_field_is_nanors_field() {
        // nanors' tables (polynomial 285): log 2 = 1, log 3 = 25, log 4 = 2.
        assert_eq!(TABLES.log[2], 1);
        assert_eq!(TABLES.log[3], 25);
        assert_eq!(TABLES.log[4], 2);
        for a in 1..=255u8 {
            assert_eq!(mul(a, inverse(a)), 1);
        }
    }

    #[test]
    fn any_two_lost_shards_come_back() {
        let data: Vec<Vec<u8>> = (0..4u8)
            .map(|i| {
                (0..37u8)
                    .map(|b| b.wrapping_mul(31).wrapping_add(i * 7))
                    .collect()
            })
            .collect();
        let parity = parity([&data[0], &data[1], &data[2], &data[3]]);
        let all: Vec<Vec<u8>> = data.iter().chain(parity.iter()).cloned().collect();
        for lost_a in 0..SHARDS {
            for lost_b in lost_a..SHARDS {
                let shards: [Option<Vec<u8>>; SHARDS] =
                    std::array::from_fn(|i| (i != lost_a && i != lost_b).then(|| all[i].clone()));
                let back = recover(&shards).unwrap();
                assert_eq!(back.to_vec(), data, "lost {lost_a} and {lost_b}");
            }
        }
    }

    #[test]
    fn three_lost_shards_do_not() {
        let shards: [Option<Vec<u8>>; SHARDS] =
            std::array::from_fn(|i| (i >= 3).then(|| vec![1, 2, 3]));
        assert!(recover(&shards).is_none());
    }
}
