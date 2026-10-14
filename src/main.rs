//! `graphlite bench <path> <edges> [nodes]`: loads random edges into a fresh
//! database and times inserts and neighbor scans.

use std::process::ExitCode;
use std::time::Instant;

use graphlite::{Db, Direction, Options};

const USAGE: &str = "usage: graphlite bench <path> <edges> [nodes]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let parsed = match args.as_slice() {
        [_, cmd, path, edges] if cmd == "bench" => edges.parse().ok().map(|e: u64| (path, e, (e / 10).max(1))),
        [_, cmd, path, edges, nodes] if cmd == "bench" => {
            edges.parse().ok().zip(nodes.parse().ok()).map(|(e, n)| (path, e, n))
        }
        _ => None,
    };
    let Some((path, edges, nodes)) = parsed else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    if std::fs::exists(path).unwrap_or(true) {
        eprintln!("{path} already exists; bench needs a fresh file");
        return ExitCode::FAILURE;
    }
    match bench(path, edges, nodes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn bench(path: &str, edges: u64, nodes: u64) -> graphlite::Result<()> {
    const PREDICATES: u64 = 4;
    let mut db = Db::open_with(path, &Options::default())?;
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut rand = move |n: u64| {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    };

    let start = Instant::now();
    for i in 0..edges {
        db.add_edge(rand(nodes), 1 + rand(PREDICATES) as u32, rand(nodes))?;
        if (i + 1) % 1_000_000 == 0 {
            println!("  {}M edges, {:.0}/s", (i + 1) / 1_000_000, (i + 1) as f64 / start.elapsed().as_secs_f64());
        }
    }
    db.commit()?;
    let load = start.elapsed();
    println!(
        "load:   {edges} edges over {nodes} nodes in {load:.2?} ({:.0} edges/s)",
        edges as f64 / load.as_secs_f64()
    );
    println!(
        "file:   {:.1} MiB ({:.1} bytes/edge)",
        db.file_size() as f64 / (1 << 20) as f64,
        db.file_size() as f64 / edges as f64
    );

    let queries = 10_000;
    let mut found = 0u64;
    let start = Instant::now();
    for _ in 0..queries {
        let dir = if rand(2) == 0 { Direction::Out } else { Direction::In };
        for n in db.neighbors(rand(nodes), 1 + rand(PREDICATES) as u32, dir)? {
            n?;
            found += 1;
        }
    }
    let scan = start.elapsed();
    println!(
        "scan:   {queries} neighbor lists in {scan:.2?} ({:.1} us/list, {:.1} neighbors avg)",
        scan.as_secs_f64() * 1e6 / queries as f64,
        found as f64 / queries as f64
    );
    let s = db.stats();
    println!("pager:  {} hits, {} reads, {} writes", s.hits, s.reads, s.writes);
    Ok(())
}
