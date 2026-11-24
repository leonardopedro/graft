//! The CLI reference under `docs/` is generated; keep it honest.

use std::{fs, path::Path};

use graft_workspace::cli_docs;

#[test]
fn committed_cli_reference_matches_clap() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(cli_docs::DOCS_DIR);
    if !dir.is_dir() {
        eprintln!("skipping: {} is not present", dir.display());
        return;
    }

    for (relative, expected) in cli_docs::render_pages() {
        let path = dir.join(&relative);
        let actual = fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "missing generated page {}: {err}\n\
                 run `cargo run -p graft-workspace --example gen-cli-docs`",
                path.display()
            )
        });
        assert_eq!(
            actual,
            expected,
            "{} is stale; run `cargo run -p graft-workspace --example gen-cli-docs`",
            path.display()
        );
    }
}
