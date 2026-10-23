//! A basic benchmark of the erasure code at realistic frame sizes:
//! `cargo run --release -p windowcast-fec --example bench`.
//!
//! Frames are cut into blocks of at most 128 data shards (or the cap given as an
//! argument) of 1154 bytes (what
//! the Direct transport does), each with parity for a given overhead. It times
//! encoding a frame and recovering it when every block has lost as many data
//! shards as it has parity, the worst case the code can still fix.

use std::time::Instant;

use windowcast_fec::Code;

const SHARD: usize = 1154;

fn blocks(n: usize, cap: usize) -> Vec<usize> {
    let count = n.div_ceil(cap).max(1);
    (0..count)
        .map(|b| n / count + usize::from(b < n % count))
        .collect()
}

fn main() {
    // The block cap is an argument so the cost of a smaller block can be seen:
    // `-- 32` caps blocks at 32 data shards. Default 128, what Direct uses.
    let cap: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(128);
    println!("blocks of at most {cap} data shards");
    println!(
        "{:>10} {:>9} {:>8} {:>12} {:>12} {:>13}",
        "frame", "overhead", "parity", "encode", "recover", "encode MB/s"
    );
    for (name, bytes) in [
        ("30 KB", 30_000usize),
        ("100 KB", 100_000),
        ("1 MB", 1_000_000),
        ("4 MB", 4_000_000),
    ] {
        let n = bytes.div_ceil(SHARD);
        for overhead in [0.05f64, 0.10, 0.25] {
            let layout = blocks(n, cap);
            let iterations = (20_000_000 / bytes).clamp(3, 2000);
            let mut parity_total = 0;
            let mut codes = Vec::new();
            let mut data = Vec::new();
            for &k in &layout {
                let m = ((k as f64 * overhead).ceil() as usize).max(1);
                parity_total += m;
                codes.push(Code::cauchy(k, m).unwrap());
                data.push(
                    (0..k)
                        .map(|i| {
                            (0..SHARD)
                                .map(|b| (b * 31 + i * 7) as u8)
                                .collect::<Vec<u8>>()
                        })
                        .collect::<Vec<_>>(),
                );
            }
            let start = Instant::now();
            let mut sink = 0usize;
            let mut parities = Vec::new();
            for _ in 0..iterations {
                parities.clear();
                for (code, block) in codes.iter().zip(&data) {
                    let refs: Vec<&[u8]> = block.iter().map(Vec::as_slice).collect();
                    let p = code.encode(&refs).unwrap();
                    sink += p[0][0] as usize;
                    parities.push(p);
                }
            }
            let encode = start.elapsed() / iterations as u32;

            // Worst case: each block loses as many data shards as it has parity.
            let inputs: Vec<Vec<Option<Vec<u8>>>> = codes
                .iter()
                .zip(&data)
                .zip(&parities)
                .map(|((code, block), parity)| {
                    let m = code.parity_shards();
                    block
                        .iter()
                        .enumerate()
                        .map(|(i, s)| (i >= m).then(|| s.clone()))
                        .chain(parity.iter().map(|p| Some(p.clone())))
                        .collect()
                })
                .collect();
            let start = Instant::now();
            for _ in 0..iterations {
                for (code, shards) in codes.iter().zip(&inputs) {
                    let back = code.recover(shards).unwrap();
                    sink += back[0][0] as usize;
                }
            }
            let recover = start.elapsed() / iterations as u32;
            let mbps = bytes as f64 / encode.as_secs_f64() / 1e6;
            println!(
                "{:>10} {:>8.0}% {:>8} {:>10.2?} {:>10.2?} {:>13.0}",
                name,
                overhead * 100.0,
                parity_total,
                encode,
                recover,
                mbps
            );
            std::hint::black_box(sink);
        }
    }
}
