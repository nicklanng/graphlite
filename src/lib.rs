//! graphlite: an embedded, single-file graph database.
//!
//! ```no_run
//! use graphlite::{Db, Direction};
//!
//! const KNOWS: u32 = 1;
//!
//! let mut db = Db::open("social.db")?;
//! let alice = db.create_node()?;
//! let bob = db.create_node()?;
//! db.add_edge(alice, KNOWS, bob)?;
//! db.commit()?;
//!
//! for friend in db.neighbors(alice, KNOWS, Direction::Out)? {
//!     println!("alice knows {}", friend?);
//! }
//! # Ok::<(), graphlite::Error>(())
//! ```
//!
//! Layers, bottom up: [`pager`] (checksummed pages and a buffer pool),
//! [`btree`] (ordered byte keys), and the graph API on [`Db`].

pub mod btree;
mod crc32;
mod error;
mod graph;
pub mod pager;
#[cfg(test)]
mod testutil;

pub use error::{Error, Result};
pub use graph::{Db, Direction, Neighbors, NodeId, Predicate};
pub use pager::{Options, Stats};
