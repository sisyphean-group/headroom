use headroom_core::profile::Profile;
use std::fs;
use std::path::Path;

#[test]
fn all_shipped_profiles_parse() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let mut names = vec![];
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        let p: Profile = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));
        let stem = path.file_stem().unwrap().to_str().unwrap();
        assert_eq!(p.name, stem, "name/filename mismatch in {}", path.display());
        names.push(p.name);
    }
    names.sort();
    eprintln!("parsed profiles: {names:?}");
    assert!(names.len() >= 14, "expected >=14 profiles, got {}", names.len());
}
