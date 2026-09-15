//! The inventory: `hosts.kdl` parsed, checked, and resolved per host
//! (vision doc section 10).
//!
//! ```no_run
//! use rustible_cli::inventory::Inventory;
//!
//! let inv = Inventory::load("hosts.kdl")?;           // every error at once
//! let web2 = inv.resolve("web2")?;                    // params + vars + sources
//! let targets = inv.select("web")?;                   // a group's hosts, or one host
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod error;
mod model;
mod parse;
mod resolve;
mod show;
mod validate;

#[cfg(test)]
mod tests;

use std::path::Path;

pub use error::{LoadError, LoadErrors, UnknownName};
pub use model::{
    Connection, Escalate, Group, Host, HostParams, Inventory, Scalar, VarBag, bag_to_json,
};
pub use resolve::{
    Conflict, ConflictKind, Overridden, ResolveError, Resolved, ResolvedParams, Source, Sources,
    local_username,
};
pub use show::render as render_show;
pub use validate::{HostResults, Severity, VarError, format_vars_report, validate};

impl Inventory {
    /// Read and parse a file. An unreadable file is one error at 1:1.
    pub fn load(path: impl AsRef<Path>) -> Result<Inventory, LoadErrors> {
        let path = path.as_ref();
        let file = path.display().to_string();
        match std::fs::read_to_string(path) {
            Ok(src) => Inventory::parse(&src, &file),
            Err(e) => Err(LoadErrors(vec![LoadError {
                file,
                line: 1,
                column: 1,
                message: format!("cannot read: {e}"),
            }])),
        }
    }

    /// Parse KDL 2.0 text. `file` only labels the errors.
    pub fn parse(src: &str, file: &str) -> Result<Inventory, LoadErrors> {
        parse::parse(src, file)
    }
}
