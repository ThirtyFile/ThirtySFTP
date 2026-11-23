//! The README that ships with the crate is a copy of the one at the root of
//! the repository. Edit the root one, then copy it over:
//! `cp README.md crates/thirtysftp/README.md`.

use std::path::Path;

#[test]
fn the_crate_readme_is_the_repository_readme() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md");
    // A packaged copy of the crate has no repository around it.
    let Ok(root) = std::fs::read_to_string(root) else {
        return;
    };
    let packaged = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .expect("crates/thirtysftp/README.md is missing");
    assert!(
        root == packaged,
        "crates/thirtysftp/README.md differs from README.md; copy the root one over it"
    );
}
