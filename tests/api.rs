//! API usage tests: exercise graphlite only through its public interface.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use graphlite::{Db, Direction, Edge, Error, Options, Value};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("graphlite-api-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }

    fn db(&self) -> PathBuf {
        self.0.join("test.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wal_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push("-wal");
    p.into()
}

/// Small pages and cache, so modest graphs exercise splits, eviction, and WAL spill.
fn tiny() -> Options {
    Options { page_size: 512, cache_pages: 16, wal_autocheckpoint: 200 }
}

#[test]
fn quickstart_social_graph() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let (alice, bob, carol) = {
        let mut db = Db::open(dir.db())?;
        let alice = db.create_node(&["Person"])?;
        let bob = db.create_node(&["Person"])?;
        let carol = db.create_node(&["Person", "Admin"])?;
        db.set_node_prop(alice, "name", "Alice")?;
        db.set_node_prop(bob, "name", "Bob")?;
        db.set_node_prop(carol, "name", "Carol")?;
        db.set_node_prop(alice, "age", 34)?;
        db.add_edge(alice, "KNOWS", bob)?;
        db.add_edge(bob, "KNOWS", carol)?;
        db.add_edge(alice, "FOLLOWS", carol)?;
        db.set_edge_prop(alice, "KNOWS", bob, "since", 2019)?;
        db.commit()?;
        (alice, bob, carol)
    };

    let mut db = Db::open(dir.db())?;
    assert_eq!(db.node_count()?, 3);
    assert_eq!(db.edge_count()?, 3);
    assert_eq!(db.node_prop(alice, "name")?, Some(Value::from("Alice")));
    assert_eq!(db.node_prop(alice, "age")?.and_then(|v| v.as_int()), Some(34));
    assert_eq!(db.neighbors(alice, "KNOWS", Direction::Out)?, [bob]);
    assert_eq!(db.neighbors(carol, "KNOWS", Direction::In)?, [bob]);
    assert_eq!(db.edge_prop(alice, "KNOWS", bob, "since")?, Some(Value::Int(2019)));
    assert_eq!(db.nodes_with_label("Admin")?, [carol]);
    assert_eq!(db.labels(carol)?, ["Person", "Admin"]);

    let names: Vec<String> = db
        .neighbors(alice, "KNOWS", Direction::Out)?
        .into_iter()
        .map(|n| Ok(db.node_prop(n, "name")?.unwrap().to_string()))
        .collect::<graphlite::Result<_>>()?;
    assert_eq!(names, ["\"Bob\""]);
    Ok(())
}

#[test]
fn nodes_and_labels() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let mut db = Db::open(dir.db())?;
    let a = db.create_node(&[])?;
    let b = db.create_node(&["City"])?;
    assert_eq!((a, b), (0, 1));
    assert!(db.node_exists(a)?);
    assert!(!db.node_exists(99)?);

    assert!(db.add_label(a, "City")?);
    assert!(!db.add_label(a, "City")?, "labels are a set");
    assert!(db.has_label(a, "City")?);
    assert!(!db.has_label(a, "Country")?);
    assert_eq!(db.nodes_with_label("City")?, [a, b]);
    assert!(db.remove_label(a, "City")?);
    assert!(!db.remove_label(a, "City")?);
    assert!(!db.remove_label(a, "NeverSeen")?);
    assert_eq!(db.nodes_with_label("City")?, [b]);
    assert!(db.nodes_with_label("NeverSeen")?.is_empty());

    assert!(matches!(db.add_label(99, "City"), Err(Error::NodeNotFound(99))));
    assert_eq!(db.node_count()?, 2);

    // Ids are never reused, even after deletes.
    assert!(db.delete_node(a)?);
    assert!(!db.delete_node(a)?);
    assert_eq!(db.create_node(&[])?, 2);
    assert_eq!(db.node_count()?, 2);
    Ok(())
}

#[test]
fn property_values_of_every_type() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let mut db = Db::open(dir.db())?;
    let n = db.create_node(&[])?;
    let big_text: String = "graph ".repeat(20_000);
    let values = [
        ("null", Value::Null),
        ("flag", Value::Bool(true)),
        ("count", Value::Int(-42)),
        ("score", Value::Float(0.125)),
        ("name", Value::from("nul\0inside")),
        ("blob", Value::Bytes(vec![0, 1, 2, 255])),
        ("essay", Value::from(big_text.as_str())),
    ];
    for (k, v) in &values {
        db.set_node_prop(n, k, v.clone())?;
    }
    db.commit()?;
    drop(db);

    let mut db = Db::open(dir.db())?;
    for (k, v) in &values {
        assert_eq!(db.node_prop(n, k)?.as_ref(), Some(v), "{k}");
    }
    let props = db.node_props(n)?;
    assert_eq!(props.len(), values.len());
    assert!(props.iter().zip(&values).all(|((k1, v1), (k2, v2))| k1 == k2 && v1 == v2));

    db.set_node_prop(n, "count", 7)?;
    assert_eq!(db.node_prop(n, "count")?, Some(Value::Int(7)));
    db.set_node_prop(n, "essay", "short now")?;
    assert_eq!(db.node_prop(n, "essay")?.unwrap().as_str(), Some("short now"));
    assert_eq!(db.remove_node_prop(n, "count")?, Some(Value::Int(7)));
    assert_eq!(db.remove_node_prop(n, "count")?, None);
    assert_eq!(db.node_prop(n, "count")?, None);
    assert_eq!(db.node_prop(n, "never-used-key")?, None);
    db.set_node_prop(n, "maybe", None::<i64>)?;
    assert_eq!(db.node_prop(n, "maybe")?, Some(Value::Null));

    assert!(matches!(db.set_node_prop(1234, "x", 1), Err(Error::NodeNotFound(1234))));
    Ok(())
}

#[test]
fn edges_types_directions_and_props() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let mut db = Db::open(dir.db())?;
    let [a, b, c] = [(); 3].map(|_| db.create_node(&[]).unwrap());

    assert!(db.add_edge(a, "LINKS", b)?);
    assert!(!db.add_edge(a, "LINKS", b)?, "one edge per (src, type, dst)");
    assert!(db.add_edge(a, "LIKES", b)?, "different types are different edges");
    assert!(db.add_edge(c, "LINKS", a)?);
    assert!(db.add_edge(a, "LINKS", a)?, "self-loops are allowed");
    assert_eq!(db.edge_count()?, 4);

    assert_eq!(db.neighbors(a, "LINKS", Direction::Out)?, [a, b]);
    assert_eq!(db.neighbors(a, "LINKS", Direction::In)?, [a, c]);
    assert_eq!(db.neighbors(a, "LINKS", Direction::Both)?, [a, b, c]);
    assert_eq!(db.neighbors(a, "UNKNOWN", Direction::Out)?, []);
    assert_eq!(db.degree(a, "LINKS", Direction::Out)?, 2);
    assert_eq!(db.degree(a, "LINKS", Direction::Both)?, 4);

    let out: HashSet<Edge> = db.edges(a, Direction::Out)?.into_iter().collect();
    let expected: HashSet<Edge> = [(a, "LINKS", b), (a, "LIKES", b), (a, "LINKS", a)]
        .map(|(src, t, dst)| Edge { src, edge_type: t.into(), dst })
        .into();
    assert_eq!(out, expected);
    assert_eq!(db.edges(a, Direction::Both)?.len(), 4, "self-loop listed once");

    db.set_edge_prop(a, "LINKS", b, "weight", 0.5)?;
    db.set_edge_prop(a, "LINKS", b, "label", "primary")?;
    assert_eq!(db.edge_prop(a, "LINKS", b, "weight")?, Some(Value::Float(0.5)));
    assert_eq!(db.edge_props(a, "LINKS", b)?.len(), 2);
    assert_eq!(db.edge_prop(a, "LIKES", b, "weight")?, None, "props are per edge");
    assert_eq!(db.remove_edge_prop(a, "LINKS", b, "label")?, Some(Value::from("primary")));
    assert!(matches!(db.set_edge_prop(b, "LINKS", a, "w", 1), Err(Error::EdgeNotFound)));
    assert!(matches!(db.set_edge_prop(a, "NOPE", b, "w", 1), Err(Error::EdgeNotFound)));
    assert!(matches!(db.add_edge(a, "LINKS", 77), Err(Error::NodeNotFound(77))));

    assert!(db.remove_edge(a, "LINKS", b)?);
    assert!(!db.remove_edge(a, "LINKS", b)?);
    assert!(!db.has_edge(a, "LINKS", b)?);
    assert!(db.has_edge(a, "LIKES", b)?);
    // Re-adding the edge does not resurrect its old properties.
    db.add_edge(a, "LINKS", b)?;
    assert_eq!(db.edge_props(a, "LINKS", b)?, []);
    Ok(())
}

#[test]
fn delete_node_detaches_everything() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let mut db = Db::open(dir.db())?;
    let hub = db.create_node(&["Hub"])?;
    let spokes: Vec<_> = (0..50).map(|_| db.create_node(&[]).unwrap()).collect();
    db.create_index("color")?;
    db.set_node_prop(hub, "color", "red")?;
    for &s in &spokes {
        db.add_edge(hub, "OUT", s)?;
        db.add_edge(s, "IN", hub)?;
        db.set_edge_prop(hub, "OUT", s, "w", 1)?;
    }
    db.add_edge(hub, "SELF", hub)?;
    assert_eq!(db.edge_count()?, 101);

    assert!(db.delete_node(hub)?);
    assert_eq!(db.edge_count()?, 0);
    assert_eq!(db.node_count()?, 50);
    assert!(db.nodes_with_label("Hub")?.is_empty());
    assert!(db.find_nodes("color", "red")?.is_empty());
    for &s in &spokes {
        assert!(db.edges(s, Direction::Both)?.is_empty());
    }
    assert_eq!(db.node_props(hub)?, []);
    Ok(())
}

#[test]
fn property_index_matches_full_scan() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let mut db = Db::open_with(dir.db(), &tiny())?;
    let nodes: Vec<_> = (0..300).map(|_| db.create_node(&[]).unwrap()).collect();
    for &n in &nodes {
        db.set_node_prop(n, "bucket", (n % 7) as i64)?;
        db.set_node_prop(n, "email", format!("user{n}@example.com"))?;
    }
    let scanned = db.find_nodes("bucket", 3)?;
    assert_eq!(scanned.len(), 43);

    db.create_index("bucket")?;
    db.create_index("email")?;
    assert_eq!(db.find_nodes("bucket", 3)?, scanned);
    assert_eq!(db.find_nodes("email", "user42@example.com")?, [42]);
    assert_eq!(db.find_nodes("bucket", Value::from("3"))?, [], "types must match");

    // The index follows updates, removals, and deletes.
    db.set_node_prop(3, "bucket", 100)?;
    db.remove_node_prop(10, "bucket")?;
    db.delete_node(17)?;
    db.commit()?;
    drop(db);

    let mut db = Db::open_with(dir.db(), &tiny())?;
    let expected: Vec<u64> = (0..300).filter(|n| n % 7 == 3 && ![3, 10, 17].contains(n)).collect();
    assert_eq!(db.find_nodes("bucket", 3)?, expected);
    assert_eq!(db.find_nodes("bucket", 100)?, [3]);
    assert!(db.drop_index("bucket")?);
    assert!(!db.drop_index("bucket")?);
    assert_eq!(db.find_nodes("bucket", 3)?, expected, "falls back to scanning");
    Ok(())
}

#[test]
fn traversals() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let mut db = Db::open(dir.db())?;
    // 0 -> 1 -> 2 -> 3 -> 4 along ROAD, plus a FERRY shortcut 0 -> 3.
    let n: Vec<_> = (0..6).map(|_| db.create_node(&[]).unwrap()).collect();
    for w in n[..5].windows(2) {
        db.add_edge(w[0], "ROAD", w[1])?;
    }
    db.add_edge(n[0], "FERRY", n[3])?;

    assert_eq!(db.shortest_path(n[0], n[4], Some("ROAD"), Direction::Out)?, Some(vec![0, 1, 2, 3, 4]));
    assert_eq!(db.shortest_path(n[0], n[4], None, Direction::Out)?, Some(vec![0, 3, 4]));
    assert_eq!(db.shortest_path(n[4], n[0], None, Direction::Out)?, None);
    assert_eq!(db.shortest_path(n[4], n[0], None, Direction::In)?, Some(vec![4, 3, 0]));
    assert_eq!(db.shortest_path(n[4], n[0], None, Direction::Both)?, Some(vec![4, 3, 0]));
    assert_eq!(db.shortest_path(n[0], n[5], None, Direction::Both)?, None, "isolated node");
    assert_eq!(db.shortest_path(n[0], n[4], Some("TRAIN"), Direction::Out)?, None);
    assert_eq!(db.shortest_path(n[2], n[2], None, Direction::Out)?, Some(vec![2]));

    let reached = db.traverse(n[0], Some("ROAD"), Direction::Out, 2)?;
    assert_eq!(reached, [(0, 0), (1, 1), (2, 2)]);
    let reached = db.traverse(n[0], None, Direction::Out, 10)?;
    assert_eq!(reached, [(0, 0), (1, 1), (3, 1), (2, 2), (4, 2)]);
    assert!(matches!(db.traverse(99, None, Direction::Out, 1), Err(Error::NodeNotFound(99))));
    Ok(())
}

#[test]
fn rollback_and_drop_discard_uncommitted_changes() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let mut db = Db::open_with(dir.db(), &tiny())?;
    let a = db.create_node(&["Keep"])?;
    db.commit()?;

    // A transaction large enough to spill pages to the WAL before rollback.
    for i in 0..500 {
        let n = db.create_node(&["Temp"])?;
        db.set_node_prop(n, "i", i)?;
        db.add_edge(a, "TEMP_EDGE", n)?;
    }
    db.set_node_prop(a, "touched", true)?;
    db.rollback();

    assert_eq!(db.node_count()?, 1);
    assert_eq!(db.edge_count()?, 0);
    assert!(db.nodes_with_label("Temp")?.is_empty(), "rolled-back labels are gone");
    assert!(db.neighbors(a, "TEMP_EDGE", Direction::Out)?.is_empty());
    assert_eq!(db.node_prop(a, "touched")?, None);
    assert_eq!(db.create_node(&[])?, 1, "id counter rolled back too");

    // Dropping without commit discards as well.
    db.add_label(a, "Uncommitted")?;
    drop(db);
    let mut db = Db::open_with(dir.db(), &tiny())?;
    assert_eq!(db.node_count()?, 1);
    assert!(!db.has_label(a, "Uncommitted")?);
    assert!(db.has_label(a, "Keep")?);
    Ok(())
}

#[derive(Debug)]
enum AppError {
    Db(Error),
    Rejected,
}

impl From<Error> for AppError {
    fn from(e: Error) -> Self {
        AppError::Db(e)
    }
}

#[test]
fn transaction_closure_commits_or_rolls_back() {
    let dir = TempDir::new();
    let mut db = Db::open(dir.db()).unwrap();

    let id = db
        .transaction(|db| -> Result<_, AppError> {
            let n = db.create_node(&["Account"])?;
            db.set_node_prop(n, "balance", 100)?;
            Ok(n)
        })
        .unwrap();

    let res = db.transaction(|db| -> Result<(), AppError> {
        db.set_node_prop(id, "balance", -50)?;
        Err(AppError::Rejected)
    });
    assert!(matches!(res, Err(AppError::Rejected)));
    assert_eq!(db.node_prop(id, "balance").unwrap(), Some(Value::Int(100)));

    let res = db.transaction(|db| -> Result<(), AppError> {
        db.add_edge(id, "OWNS", 404)?;
        Ok(())
    });
    assert!(matches!(res, Err(AppError::Db(Error::NodeNotFound(404)))));

    drop(db);
    let mut db = Db::open(dir.db()).unwrap();
    assert_eq!(db.node_prop(id, "balance").unwrap(), Some(Value::Int(100)));
}

/// Simulates crashes by copying the database and WAL while open, then
/// truncating the copied WAL at many points: every prefix must open to
/// exactly the state of some commit.
#[test]
fn crash_recovery_yields_a_committed_state() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let opts = Options { wal_autocheckpoint: u64::MAX, ..tiny() };
    let mut db = Db::open_with(dir.db(), &opts)?;
    let mut committed_counts = vec![0u64];
    for batch in 0..4u64 {
        for i in 0..40 {
            let n = db.create_node(&["Batch"])?;
            db.set_node_prop(n, "batch", batch as i64)?;
            if n > 0 {
                db.add_edge(n - 1, "NEXT", n)?;
            }
            db.set_node_prop(n, "i", i)?;
        }
        db.commit()?;
        committed_counts.push(db.node_count()?);
    }
    // An uncommitted tail that must never be visible.
    for _ in 0..40 {
        db.create_node(&["Batch"])?;
    }

    let crash_dir = TempDir::new();
    let wal = std::fs::read(wal_path(&dir.db()))?;
    let main = std::fs::read(dir.db())?;
    let frame = 24 + 512;
    let mut cuts: Vec<usize> = (32..wal.len()).step_by(frame).flat_map(|c| [c, c + 7, c + 300]).collect();
    cuts.push(wal.len());
    let mut seen = BTreeSet::new();
    for cut in cuts.into_iter().filter(|&c| c <= wal.len()) {
        std::fs::write(crash_dir.db(), &main)?;
        std::fs::write(wal_path(&crash_dir.db()), &wal[..cut])?;
        let mut db = Db::open_with(crash_dir.db(), &opts)?;
        let count = db.node_count()?;
        assert!(committed_counts.contains(&count), "cut {cut}: {count} nodes is not a committed state");
        // The state is internally consistent, not just a valid count.
        assert_eq!(db.nodes_with_label("Batch")?.len() as u64, count);
        assert_eq!(db.edge_count()?, count.saturating_sub(1));
        if count > 0 {
            let last = count - 1;
            assert_eq!(db.node_prop(last, "i")?, Some(Value::Int(39)));
        }
        seen.insert(count);
        db.close()?;
    }
    assert!(seen.len() >= 3, "cuts should land in several commits: {seen:?}");
    assert!(seen.contains(&160), "the full WAL recovers the last commit");
    Ok(())
}

#[test]
fn garbage_after_the_wal_is_ignored() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let opts = Options { wal_autocheckpoint: u64::MAX, ..tiny() };
    let mut db = Db::open_with(dir.db(), &opts)?;
    db.create_node(&["A"])?;
    db.commit()?;

    // Snapshot the files as a crash would leave them, plus trailing garbage.
    let crash_dir = TempDir::new();
    let mut wal = std::fs::read(wal_path(&dir.db()))?;
    assert!(wal.len() > 32, "the commit should still be in the WAL");
    wal.extend(std::iter::repeat_n(0xAB, 5_000));
    std::fs::write(wal_path(&crash_dir.db()), wal)?;
    std::fs::copy(dir.db(), crash_dir.db())?;

    let mut db = Db::open_with(crash_dir.db(), &opts)?;
    assert_eq!(db.nodes_with_label("A")?, [0]);
    Ok(())
}

#[test]
fn corruption_is_detected_not_returned() -> graphlite::Result<()> {
    let dir = TempDir::new();
    {
        let mut db = Db::open(dir.db())?;
        let n = db.create_node(&["X"])?;
        db.set_node_prop(n, "k", "v")?;
        db.commit()?;
        db.close()?;
    }
    let mut bytes = std::fs::read(dir.db())?;
    bytes[4096 + 200] ^= 0x40; // the tree root, page 1
    std::fs::write(dir.db(), bytes)?;
    let mut db = Db::open(dir.db())?;
    assert!(matches!(db.node_count(), Err(Error::Corrupt(_))));
    Ok(())
}

#[test]
fn one_handle_per_file() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let db = Db::open(dir.db())?;
    assert!(matches!(Db::open(dir.db()), Err(Error::Locked)));
    db.close()?;
    Db::open(dir.db())?;
    Ok(())
}

#[test]
fn db_can_move_across_threads() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let mut db = Db::open(dir.db())?;
    let handle = std::thread::spawn(move || {
        let n = db.create_node(&["FromThread"]).unwrap();
        db.commit().unwrap();
        (db, n)
    });
    let (mut db, n) = handle.join().unwrap();
    assert_eq!(db.nodes_with_label("FromThread")?, [n]);
    Ok(())
}

/// A larger random graph with many commits and reopens, checked against an
/// in-memory model.
#[test]
fn random_graph_matches_model_across_reopens() -> graphlite::Result<()> {
    let dir = TempDir::new();
    let mut rng = 0x1234_5678_9ABC_DEF1u64;
    let mut rand = move |n: u64| {
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    };
    let types = ["A", "B", "C"];
    let nodes = 400u64;
    let mut model: BTreeSet<(u64, &str, u64)> = BTreeSet::new();

    let mut db = Db::open_with(dir.db(), &tiny())?;
    for _ in 0..nodes {
        db.create_node(&[])?;
    }
    for round in 0..6 {
        for _ in 0..1_500 {
            let (s, t, d) = (rand(nodes), types[rand(3) as usize], rand(nodes));
            if rand(4) == 0 {
                assert_eq!(db.remove_edge(s, t, d)?, model.remove(&(s, t, d)));
            } else {
                assert_eq!(db.add_edge(s, t, d)?, model.insert((s, t, d)));
            }
        }
        db.commit()?;
        if round % 2 == 1 {
            db.close()?;
            db = Db::open_with(dir.db(), &tiny())?;
        }
    }

    assert_eq!(db.edge_count()?, model.len() as u64);
    for node in 0..nodes {
        for t in types {
            let out: Vec<u64> = model.iter().filter(|e| e.0 == node && e.1 == t).map(|e| e.2).collect();
            let mut inc: Vec<u64> = model.iter().filter(|e| e.2 == node && e.1 == t).map(|e| e.0).collect();
            inc.sort_unstable();
            assert_eq!(db.neighbors(node, t, Direction::Out)?, out);
            assert_eq!(db.neighbors(node, t, Direction::In)?, inc);
        }
    }
    Ok(())
}
