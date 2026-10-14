# graphlite

**An embedded, single-file graph database in Rust: SQLite's deployment model,
built for graphs.**

> [!WARNING]
> **Experimental. Don't use this for data you care about.** graphlite is an
> experiment in building a graph storage engine from first principles. The
> API, the on-disk format and the WAL format will change without migration
> paths. It isn't published on crates.io, nothing runs it in production, and
> it has had one audit and no outside review. Use it to read, learn from, or
> experiment with, not to store anything you can't regenerate.

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

## What it is

graphlite is a library, not a server. You link it into your program and it
stores a property graph in one file on disk, plus a write-ahead log beside it,
much as SQLite stores tables. There is no process to run, no network protocol
and no configuration. Everything up to the Rust standard library is written
here: the pager, buffer pool, B+tree, write-ahead log, checksums and graph
layer. It has no dependencies.

The data model is a **property graph**:

- **Nodes** have dense `u64` ids, any number of labels (`Person`, `Admin`) and
  typed properties.
- **Edges** are directed and typed (`KNOWS`, `FOLLOWS`) and carry their own
  properties.
- **Property values** are null, bool, int, float, string or bytes, of any size.

The goal is to find out what a SQLite-style engine looks like when it's
designed around adjacency rather than tables. Neighbor lists, in either
direction, are a single range scan instead of a join.

## Status

What works today:

- **Nodes and labels**: create, delete (which also detaches every edge),
  add and remove labels, and `nodes_with_label`.
- **Properties** on nodes and edges, with `find_nodes(key, value)`. It uses a
  property index if you've created one with `create_index`, and a full scan
  otherwise.
- **Traversals**: `neighbors`, `edges`, `degree`, breadth-first `traverse`
  with a depth limit, and two-ended `shortest_path`. Each can filter by edge
  type and follow edges out, in, or both ways.
- **Transactions**: every change since the last commit forms one atomic,
  durable unit. You get `commit`, `rollback`, and `transaction(|db| …)`,
  which commits on `Ok` and rolls back on `Err`.
- **Crash safety**: a crash at any point reopens to exactly the last commit,
  and damaged files are reported as `Error::Corrupt`. See
  [Durability](#durability).

Known limitations, several of which are on the [roadmap](#roadmap):

- **One handle per file.** The file is locked while it's open, and every
  method takes `&mut self`, so reads are serialized too. There are no
  concurrent readers yet; SQLite allows them.
- **No query language.** You call Rust methods. There are no bindings for
  other languages yet.
- **At most one edge per (source, type, destination).** Parallel edges of the
  same type aren't supported.
- **The file never shrinks.** Deletes don't rebalance or free B+tree pages.
- **Uncommitted changes are discarded on drop.** Nothing autocommits, so call
  `commit()`.
- **Results are returned as `Vec`s.** A node with millions of edges is
  materialized in memory.
- **No stable format.** A database written by one commit of graphlite may not
  open with another.

## Trying it

graphlite needs Rust 1.89 or newer. It isn't on crates.io, so depend on the
repository:

```toml
[dependencies]
graphlite = { git = "https://github.com/nicklanng/graphlite" }
```

Pin a `rev` if you want builds to stay reproducible: the API changes freely.

`Db::open(path)` creates the database if it doesn't exist. `Db::open_with`
takes `Options` for the page size (new files only), the buffer pool size, and
the WAL autocheckpoint threshold. `Db` is `Send`, so to share one across
threads, put it behind a `Mutex`.

## Durability

- **Write-ahead log.** Changes are written to `<file>-wal`, never over the
  main file. A commit appends the header page as a commit frame and makes one
  fsync. Once the log passes `wal_autocheckpoint` frames, committed pages are
  copied into the main file and the log is reset.
- **Recovery** replays the log up to the last valid commit frame. Each commit
  frame carries a checksum over its whole transaction, chained from the
  previous commit. So frames left behind by a transaction that never
  committed can't be replayed later, even after a crash persists them out of
  order.
- **Corruption.** Every page ends in a CRC-32C, verified on every read. Tree
  pages are also checked for structure when they're loaded, and walks over
  the tree are bounded. A damaged or hostile file therefore gives
  `Error::Corrupt` rather than a panic, a hang, or silently wrong data.
- **Failed writes.** A write that fails partway, such as an I/O error halfway
  through `delete_node`, may leave the transaction half-applied. Writes and
  commits then return `Error::RollbackRequired` until you call `rollback`.
  `transaction(…)` does this for you.
- **A failed commit can still be durable.** If `commit` returns an error, the
  commit may still have reached the disk, so a crash can reveal it. SQLite
  has the same ambiguity.

## How it works

| Layer | File | What it does |
|---|---|---|
| Graph API | `src/graph.rs` | Nodes, labels, properties, edges, indexes and traversals, over tagged B+tree keys |
| Values | `src/value.rs` | Property values in an order-preserving, self-delimiting encoding |
| B+tree | `src/btree.rs` | Slotted pages, variable-length keys, overflow chains for large values, cursors |
| Pager | `src/pager.rs` | Checksummed pages, a clock-eviction buffer pool, freelist, commit, rollback, checkpoint |
| WAL | `src/wal.rs` | Frame log with salted, chained checksums, recovery, and checkpoint reset |
| CRC | `src/crc32.rs` | CRC-32C on ARMv8 and SSE4.2 instructions, with a table fallback |
| Bench | `src/main.rs` | `graphlite bench <path> <edges> [nodes]` |

The whole graph lives in **one B+tree** under tagged keys. Integers are
big-endian, so byte order is numeric order, and each adjacency list is one
contiguous key range:

```text
N | node                          node exists
L | node | label                  node's labels
l | label | node                  label index
P | node | key          → value   node property
E | src | type | dst              out-edge
e | dst | type | src              in-edge
Q | src | type | dst | key → value  edge property
X | key | value | node            property index entry
I | key                           property `key` is indexed
Y | kind | name         → id      symbol table (labels, edge types, keys)
y | kind | id           → name
M | name                → u64     counters
```

Labels, edge types and property keys are interned as `u32` symbols. Every
edge is stored twice, once under each endpoint, so in-neighbors are as cheap
as out-neighbors. The cost is about 83 bytes per edge, including B+tree
overhead.

## Performance

These numbers come from an M-series laptop with 4 KiB pages, a 16 MiB cache,
2 million random edges over 200,000 nodes, and `--release`. The graph is ten
times larger than the cache, so most reads go to disk. Run
`graphlite bench <path> 2000000`, then
`cargo run --release --example read_bench -- <path>`:

```
load:   200000 nodes, 2000000 edges in 12.87s (155455 edges/s, commit every 100000)
size:   157.6 MiB (82.6 bytes/edge)
neighbors:     1000000 in 1.30s (1.30 us each)
2-hop traverse: 10000 in 168.61ms (16.86 us each, 110 nodes avg)
shortest path: 1000 in 571.00ms (0.57 ms each, 5.4 nodes avg)
```

A profiling pass with [samply](https://github.com/mstange/samply) made loads
7× faster and neighbor lists 8× faster than the first version:

- **CRC-32C on the CPU's CRC instructions.** Checksums had been about 78% of
  load time. They now run three interleaved streams per page.
- **Fewer B+tree descents per edge.** `add_edge` went from 7 to 4.
- **Shortest path searches from both ends.**
- **Smaller wins:** a cheaper hasher for page maps, key comparison 8 bytes at
  a time, and checkpoints that reuse cached pages.

Most of the remaining time is system calls on cache misses and fsyncs.
Checking page structure on load costs about 8% on these cold reads.

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
cargo test                     # 41 tests: 23 unit and 18 public-API
cargo clippy --all-targets
cargo run --release -- bench /tmp/g.db 1000000
```

The public-API tests in `tests/api.rs` include crash simulations. They cut
the WAL at dozens of points, tear a commit so that only some of its frames
reach disk, add trailing garbage, and flip bits in pages. Every case must
reopen to a consistent, committed state or report corruption. There is also a
random graph checked against an in-memory model across reopens.

## License

MIT. See [LICENSE](LICENSE).
