# graphlite

An embedded, single-file graph database: SQLite's deployment model, built for
graphs. Pure Rust with no dependencies.

```rust
use graphlite::{Db, Direction};

let mut db = Db::open("social.db")?;
let alice = db.create_node(&["Person"])?;
let bob = db.create_node(&["Person"])?;
db.set_node_prop(alice, "name", "Alice")?;
db.set_node_prop(bob, "name", "Bob")?;
db.add_edge(alice, "KNOWS", bob)?;
db.set_edge_prop(alice, "KNOWS", bob, "since", 2019)?;
db.commit()?;

for friend in db.neighbors(alice, "KNOWS", Direction::Out)? {
    println!("alice knows {:?}", db.node_prop(friend, "name")?);
}
```

## What's here

- **Nodes** with dense u64 ids, any number of labels, and typed properties:
  null, bool, int, float, string and bytes, with values of any size.
- **Edges** are directed and typed (`KNOWS`, `FOLLOWS`, …) and carry their own
  properties. There is at most one edge per (source, type, destination).
  Neighbor lists in either direction are a single range scan.
- **Lookups**: `nodes_with_label`, and `find_nodes(key, value)`, which uses a
  property index when you create one with `create_index` and scans otherwise.
- **Traversals**: `neighbors`, `edges`, `degree`, breadth-first `traverse`
  with a depth limit, and `shortest_path`. Each can filter by edge type and
  follow edges out, in, or both ways.
- **Transactions**: every change since the last commit forms one atomic,
  durable unit. You get `commit`, `rollback`, and `transaction(|db| …)`,
  which commits on `Ok` and rolls back on `Err`. Dropping the `Db` discards
  uncommitted changes.
- **Crash safety**: changes go to a write-ahead log (`<file>-wal`), and a
  commit is a single fsync. After a crash, the database reopens in exactly
  the state of the last commit. Every page carries a CRC-32C, so corruption
  surfaces as `Error::Corrupt` rather than as wrong data.
- **One process, one handle**: the file is locked while it's open. `Db` is
  `Send`; share it across threads behind a `Mutex`.

## Layout

| Layer | File | What it does |
|---|---|---|
| Graph API | `src/graph.rs` | Nodes, labels, properties, edges, indexes, traversals over tagged B+tree keys |
| Values | `src/value.rs` | Property values in an order-preserving, self-delimiting encoding |
| B+tree | `src/btree.rs` | Slotted pages, variable-length keys, overflow chains for large values, cursors |
| Pager | `src/pager.rs` | Checksummed pages, clock-eviction buffer pool, freelist, commit, rollback, checkpoint |
| WAL | `src/wal.rs` | Frame log with salted checksums, recovery, and checkpoint reset |
| Bench | `src/main.rs` | `graphlite bench <path> <edges> [nodes]` |

## Tests

`cargo test` runs 31 tests. The 16 unit tests cover each layer, including a
randomized B+tree test against a reference `BTreeMap`. The 15 tests in
`tests/api.rs` use only the public API and cover:

- every API area, plus error cases
- crash recovery: the WAL is truncated at dozens of points, and every cut
  must reopen to a consistent, committed state
- trailing garbage in the WAL
- page corruption
- file locking
- a random graph checked against an in-memory model across reopens

## Performance

M-series laptop, 4 KiB pages, 16 MiB cache, random edges, `cargo build --release`.
Run `graphlite bench <path> 2000000`, then
`cargo run --release --example read_bench -- <path>`:

```
load:   200000 nodes, 2000000 edges in 13.78s (145178 edges/s, commit every 100000)
size:   157.6 MiB (82.6 bytes/edge)
neighbors:     1000000 in 1.15s (1.15 us each)
2-hop traverse: 10000 in 151.97ms (15.20 us each, 110 nodes avg)
shortest path: 1000 in 523.96ms (0.52 ms each, 5.4 nodes avg)
```

A profiling pass with [samply](https://github.com/mstange/samply) gave these
gains over the first version on the same workload:

| | before | after | change |
|---|---|---|---|
| Load | 20.8k edges/s | 145k edges/s | 7× |
| Neighbor list | 9.1 µs | 1.15 µs | 8× |
| Shortest path (1M edges) | 20 ms | 0.25 ms | 80× |

Where the time went, and the fixes:

- **Page checksums (about 78% of load time).** CRC-32 was computed one byte
  at a time, twice per WAL write. Now it's CRC-32C on the CPU's CRC
  instructions, with three interleaved streams per page and one checksum per
  write.
- **Redundant tree lookups.** `add_edge` used 7 descents per edge; it now
  uses 4. Counters are cached and written at commit, and an exists-check plus
  insert became a single `put_if_absent`.
- **Shortest path.** It now searches from both ends instead of one, and scans
  keys without allocating.
- **Smaller costs:** SipHash in page maps is replaced by a multiply-fold
  hasher, key comparison is done 8 bytes at a time, and checkpoints reuse
  cached pages instead of re-reading the WAL.

Most of the remaining time is system calls for cache misses (`pread`,
`pwrite`) and fsyncs. The graph is ten times larger than the cache.
Compressed adjacency lists (a smaller working set), a bulk loader, and
mmap-based reads are the next levers. Of the CPU time that's left, B+tree
binary search takes the largest share.

## Roadmap

- [ ] Bulk loader: external sort, then build pages bottom-up
- [ ] Block-compressed adjacency lists, targeting under 10 bytes per edge
- [ ] Streaming iterators for very high-degree nodes
- [ ] Concurrent readers, using snapshot reads from the WAL
- [ ] Rebalance B+tree pages on delete, and shrink the file
- [ ] Range queries on property indexes (the encoding already sorts correctly)
- [ ] C ABI, then Python bindings (PyO3) and Node bindings (napi-rs)
- [ ] An openCypher subset with worst-case optimal joins
- [ ] Benchmarks on Twitter-2010 and LDBC SNB against Kùzu forks, DuckPGQ and SQLite

## Development

```sh
cargo test
cargo clippy --all-targets
cargo run --release -- bench /tmp/g.db 1000000
```
