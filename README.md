# graphlite

An embedded, single-file graph database: SQLite's deployment model, built for
graphs. Pure Rust with no dependencies.

> Status: early scaffold. The storage engine works and is tested, but there is
> no WAL yet, so a crash between commits can corrupt the file.

```rust
use graphlite::{Db, Direction};

const KNOWS: u32 = 1;

let mut db = Db::open("social.db")?;
let alice = db.create_node()?;
let bob = db.create_node()?;
db.add_edge(alice, KNOWS, bob)?;
db.commit()?;

for friend in db.neighbors(alice, KNOWS, Direction::Out)? {
    println!("alice knows {}", friend?);
}
```

## Layout

| Layer | File | What it does |
|---|---|---|
| Pager | `src/pager.rs` | Fixed-size pages in one file, CRC-32 per page, clock-eviction buffer pool, freelist |
| B+tree | `src/btree.rs` | Slotted pages, variable-length byte keys and values, splits, cursors |
| Graph | `src/graph.rs` | Dense u64 node ids; edges stored in both directions as `(tag, pred, node, neighbor)` keys, so a neighbor list is one range scan |
| Bench | `src/main.rs` | `graphlite bench <path> <edges> [nodes]` |

## Roadmap

1. **Storage you can trust**
   - [x] Pager, checksums, buffer pool
   - [x] B+tree, tested against a reference model
   - [ ] WAL with atomic commit and snapshot readers
   - [ ] Crash-injection tests
   - [ ] Delete rebalancing
   - [ ] Overflow pages for large values
2. **Graph layer**
   - [x] Edges and neighbor scans in both directions
   - [ ] Properties and secondary indexes
   - [ ] Labels
   - [ ] Block-compressed posting lists (delta + bit-packing)
   - [ ] C ABI
3. **Scale**
   - [ ] Bulk loader: external sort, bottom-up page build
   - [ ] Benchmarks on Twitter-2010 and LDBC SNB against Kùzu forks, DuckPGQ and SQLite
4. **Queries**
   - [ ] openCypher subset
   - [ ] Planner with worst-case optimal joins over sorted posting lists
   - [ ] Path and shortest-path operators

## Baseline numbers

M-series laptop, 4 KiB pages, 16 MiB cache, random edges, `cargo build --release`:

```
load:   2000000 edges over 200000 nodes in 47.24s (42336 edges/s)
file:   152.0 MiB (79.7 bytes/edge)
scan:   10000 neighbor lists in 80.26ms (8.0 us/list, 2.5 neighbors avg)
```

What these numbers say:

- **Loading is slow.** Random inserts into a tree ten times larger than the
  cache thrash it. That's the bulk loader's job.
- **Files are big.** Each edge costs two uncompressed 21-byte keys in
  two-thirds-full pages. Posting-list compression should bring this under
  10 bytes per edge.

## Development

```sh
cargo test
cargo clippy --all-targets
cargo run --release -- bench /tmp/g.db 1000000
```
