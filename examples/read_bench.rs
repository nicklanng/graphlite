//! Read-path benchmark over a database built by `graphlite bench`:
//! `cargo run --release --example read_bench -- <path> [queries]`

use std::time::Instant;

use graphlite::{Db, Direction};

fn main() -> graphlite::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: read_bench <path> [queries]");
    let queries: u64 = args.get(2).and_then(|q| q.parse().ok()).unwrap_or(1_000_000);
    let mut db = Db::open(path)?;
    let nodes = db.node_count()?;
    let types = ["T0", "T1", "T2", "T3"];
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut rand = move |n: u64| {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    };

    let start = Instant::now();
    let mut found = 0;
    for _ in 0..queries {
        let dir = if rand(2) == 0 { Direction::Out } else { Direction::In };
        found += db.neighbors(rand(nodes), types[rand(4) as usize], dir)?.len();
    }
    let t = start.elapsed();
    println!("neighbors:     {queries} in {t:.2?} ({:.2} us each, {found} results)", t.as_secs_f64() * 1e6 / queries as f64);

    let start = Instant::now();
    let mut reached = 0;
    let n = queries / 100;
    for _ in 0..n {
        reached += db.traverse(rand(nodes), None, Direction::Out, 2)?.len();
    }
    let t = start.elapsed();
    println!("2-hop traverse: {n} in {t:.2?} ({:.2} us each, {} nodes avg)", t.as_secs_f64() * 1e6 / n as f64, reached as u64 / n);

    let start = Instant::now();
    let mut lens = 0;
    let n = queries / 1000;
    for _ in 0..n {
        lens += db.shortest_path(rand(nodes), rand(nodes), None, Direction::Both)?.map_or(0, |p| p.len());
    }
    let t = start.elapsed();
    println!("shortest path: {n} in {t:.2?} ({:.2} ms each, {:.1} nodes avg)", t.as_secs_f64() * 1e3 / n as f64, lens as f64 / n as f64);
    let s = db.stats();
    println!("pager: {} hits, {} reads", s.hits, s.reads);
    Ok(())
}
