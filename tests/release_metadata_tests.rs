#[test]
fn cargo_toml_declares_publication_metadata() {
    let manifest = std::fs::read_to_string("Cargo.toml").unwrap();
    assert!(manifest.contains("version = \"0.2.0\""));
    assert!(manifest.contains("license = \"MIT\""));
    assert!(manifest.contains("repository = \"https://github.com/LEMNXCIX/llama-r\""));
    assert!(manifest.contains("description ="));
    assert!(std::path::Path::new("LICENSE").exists());
    let changelog = std::fs::read_to_string("CHANGELOG.md").unwrap();
    assert!(changelog.contains("## [0.2.0] - 2026-11-25"));
}
