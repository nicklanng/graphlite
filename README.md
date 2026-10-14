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
  the state of the last commit. Every page carries a CRC-32, so corruption
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

## Baseline numbers

M-series laptop, 4 KiB pages, 16 MiB cache, random edges, committing every
100k edges, `cargo build --release`:

```
load:   200000 nodes, 2000000 edges in 96.12s (20807 edges/s, commit every 100000)
size:   157.8 MiB (82.7 bytes/edge)
scan:   10000 neighbor lists in 90.94ms (9.1 us/list, 2.5 neighbors avg)
```

Loading is the weak spot. Random inserts into a tree ten times larger than
the cache keep evicting pages, and each edge also checks that both nodes
exist. Files are also large: each edge is stored as two uncompressed keys.

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
