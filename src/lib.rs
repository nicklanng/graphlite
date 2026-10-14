//! graphlite: an embedded, single-file graph database.
//!
//! ```no_run
//! use graphlite::{Db, Direction};
//!
//! let mut db = Db::open("social.db")?;
//! let alice = db.create_node(&["Person"])?;
//! let bob = db.create_node(&["Person"])?;
//! db.set_node_prop(alice, "name", "Alice")?;
//! db.set_node_prop(bob, "name", "Bob")?;
//! db.add_edge(alice, "KNOWS", bob)?;
//! db.set_edge_prop(alice, "KNOWS", bob, "since", 2019)?;
//! db.commit()?;
//!
//! for friend in db.neighbors(alice, "KNOWS", Direction::Out)? {
//!     println!("alice knows {:?}", db.node_prop(friend, "name")?);
//! }
//! # Ok::<(), graphlite::Error>(())
//! ```
//!
//! Layers, bottom up: [`pager`] (checksummed pages, a buffer pool, and
//! transactions over a write-ahead log), [`btree`] (ordered byte keys), and
//! the graph API on [`Db`].

pub mod btree;
mod crc32;
mod error;
mod graph;
mod io;
pub mod pager;
#[cfg(test)]
mod testutil;
mod value;
mod wal;

pub use error::{Error, Result};
pub use graph::{Db, Direction, Edge, NodeId};
pub use pager::{Options, Stats};
pub use value::Value;
