//! The npm packages and the crate publish one release, so they carry one version.
#[test]
fn crate_version_matches_package_json() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../package.json")).unwrap();
    let package: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(package["version"], env!("CARGO_PKG_VERSION"), "bump crates/diffle/Cargo.toml with package.json");
}
