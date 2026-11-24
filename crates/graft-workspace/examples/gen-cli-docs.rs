//! Regenerates the CLI reference pages in `docs/`.
//!
//! ```sh
//! cargo run -p graft-workspace --example gen-cli-docs
//! ```

use std::{env, path::PathBuf};

use graft_workspace::cli_docs;

fn main() -> anyhow::Result<()> {
    let dir = match env::args_os().nth(1) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(cli_docs::DOCS_DIR),
    };

    let written = cli_docs::write_docs(&dir)?;
    println!("wrote {} pages to {}", written.len(), dir.display());
    Ok(())
}
