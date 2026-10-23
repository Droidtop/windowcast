//! Reed-Solomon parity for GameStream audio: four data packets, two parity
//! packets, over GF(2^8) with the polynomial 0x11d. The parity matrix is
//! the fixed one Sunshine and moonlight-common-c both install (the matrix
//! NVIDIA's hosts used, taken from OpenFEC): parity `j` is the sum over the
//! data shards `i` of `PARITY[j][i]` times shard `i`.
//!
//! The arithmetic and the recovery are in `windowcast-fec`, shared with the
//! Direct transport; only the fixed matrix and the 4+2 shape are GameStream's.

use std::sync::OnceLock;

use windowcast_fec::Code;

pub const DATA_SHARDS: usize = 4;
pub const PARITY_SHARDS: usize = 2;
pub const SHARDS: usize = DATA_SHARDS + PARITY_SHARDS;

const PARITY: [[u8; DATA_SHARDS]; PARITY_SHARDS] =
    [[0x77, 0x40, 0x38, 0x0e], [0xc7, 0xa7, 0x0d, 0x6c]];

fn code() -> &'static Code {
    static CODE: OnceLock<Code> = OnceLock::new();
    CODE.get_or_init(|| {
        Code::new(DATA_SHARDS, PARITY.iter().map(|row| row.to_vec()).collect())
            .expect("the fixed matrix is 2 rows of 4")
    })
}

/// The two parity shards for four data shards of one size.
pub fn parity(data: [&[u8]; DATA_SHARDS]) -> [Vec<u8>; PARITY_SHARDS] {
    let mut parity = code()
        .encode(&data)
        .expect("four data shards of one size")
        .into_iter();
    std::array::from_fn(|_| parity.next().expect("two parity shards"))
}

/// The four data shards from any four of the six (all one size); `None`
/// when fewer than four arrived.
pub fn recover(shards: &[Option<Vec<u8>>; SHARDS]) -> Option<[Vec<u8>; DATA_SHARDS]> {
    let mut data = code().recover(shards)?.into_iter();
    Some(std::array::from_fn(|_| {
        data.next().expect("four data shards")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parity this code made before it moved into windowcast-fec, for
    /// fixed input: if these change, stock Moonlight can no longer read our
    /// audio.
    #[test]
    fn parity_is_what_it_was_before_the_split() {
        let data: Vec<Vec<u8>> = (0..4u8)
            .map(|i| {
                (0..16u8)
                    .map(|b| b.wrapping_mul(31).wrapping_add(i * 7))
                    .collect()
            })
            .collect();
        let p = parity([&data[0], &data[1], &data[2], &data[3]]);
        assert_eq!(
            p[0],
            [70, 194, 139, 88, 177, 70, 159, 145, 148, 182, 50, 44, 143, 50, 198, 241]
        );
        assert_eq!(
            p[1],
            [91, 151, 71, 76, 36, 19, 10, 105, 96, 234, 136, 49, 146, 110, 75, 247]
        );
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
