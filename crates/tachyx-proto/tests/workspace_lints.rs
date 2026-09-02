//! Every crate must opt in to the workspace lints.
//!
//! Cargo's workspace lint table does nothing unless a package asks for it, so a
//! crate that omits the opt-in silently loses `forbid(unsafe_code)`, the pedantic
//! set, and the documentation requirement. Nothing else catches that.

use std::path::Path;

/// The crates allowed to contain unsafe, and why each one is.
///
/// `tachyx-unsafe` holds every raw handle a platform hands across: a buffer
/// descriptor GStreamer takes, the OpenGL context Flutter's Linux embedder has
/// made current, and the Java surface its Android producer offers. No type can
/// prove any of the three names a live thing.
///
/// `tachyx-client-ffi` holds the code `flutter_rust_bridge` generates, where
/// every call across the Dart boundary reads a pointer Dart wrote.
///
/// Both keep the rest of the workspace lint set, which the first test below
/// checks, and neither holds any decision of its own.
const UNSAFE_EXCEPTIONS: [&str; 2] = ["tachyx-unsafe", "tachyx-client-ffi"];

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
        let name = manifest
            .parent()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
            .expect("crate directory name");

        if UNSAFE_EXCEPTIONS.contains(&name) {
            // A crate that cannot take the workspace table, because that table
            // forbids unsafe. It must still carry the rest of the set.
            for lint in [
                "missing_docs = \"deny\"",
                "missing_debug_implementations = \"deny\"",
                "unreachable_pub = \"deny\"",
                "pedantic = { level = \"deny\", priority = -1 }",
            ] {
                assert!(
                    text.contains(lint),
                    "{} is exempt from the workspace lint table but drops `{lint}` with it",
                    manifest.display()
                );
            }
        } else {
            assert!(
                normalised.contains("[lints] workspace = true"),
                "{} does not opt in to the workspace lints, so it is not linted at all",
                manifest.display()
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "no crates were checked");
}

#[test]
fn only_the_named_crates_may_lift_the_ban_on_unsafe() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory");
    for entry in std::fs::read_dir(crates).expect("crates directory is readable") {
        let path = entry.expect("readable entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("crate directory name");
        if UNSAFE_EXCEPTIONS.contains(&name) {
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
                "{} tries to lift the ban on unsafe; only {UNSAFE_EXCEPTIONS:?} may",
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
