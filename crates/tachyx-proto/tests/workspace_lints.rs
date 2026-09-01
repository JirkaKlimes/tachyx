//! Every crate must opt in to the workspace lints.
//!
//! Cargo's workspace lint table does nothing unless a package asks for it, so a
//! crate that omits the opt-in silently loses `forbid(unsafe_code)`, the pedantic
//! set, and the documentation requirement. Nothing else catches that.

use std::path::Path;

/// The one crate allowed to contain unsafe, and why.
const UNSAFE_EXCEPTION: &str = "tachyx-dmabuf";

#[test]
fn every_crate_opts_in_to_the_workspace_lints() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory");
    let mut checked = 0;
    for entry in std::fs::read_dir(crates).expect("crates directory is readable") {
        let manifest = entry.expect("readable entry").path().join("Cargo.toml");
        if !manifest.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest).expect("manifest is readable");
        let normalised: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            normalised.contains("[lints] workspace = true"),
            "{} does not opt in to the workspace lints, so it is not linted at all",
            manifest.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no crates were checked");
}

#[test]
fn only_one_crate_may_lift_the_ban_on_unsafe() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory");
    for entry in std::fs::read_dir(crates).expect("crates directory is readable") {
        let path = entry.expect("readable entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("crate directory name");
        if name == UNSAFE_EXCEPTION {
            continue;
        }
        let source = path.join("src");
        if !source.exists() {
            continue;
        }
        for file in walk(&source) {
            let text = std::fs::read_to_string(&file).expect("source is readable");
            assert!(
                !text.contains("allow(unsafe_code)") && !text.contains("expect(unsafe_code)"),
                "{} tries to lift the ban on unsafe; only {UNSAFE_EXCEPTION} may",
                file.display()
            );
        }
    }
}

fn walk(directory: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_owned()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(next).expect("directory is readable") {
            let path = entry.expect("readable entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    found
}
