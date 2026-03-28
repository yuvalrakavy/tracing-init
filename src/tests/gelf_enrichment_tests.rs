use serde_json::Map;

#[test]
fn test_gelf_includes_service_name() {
    let mut fields = Map::new();
    crate::gelf::add_service_field(&mut fields, Some("my-service"));
    assert_eq!(
        fields.get("_service").and_then(|v| v.as_str()),
        Some("my-service")
    );
}

#[test]
fn test_gelf_includes_target() {
    let mut fields = Map::new();
    crate::gelf::add_metadata_fields(
        &mut fields,
        Some("my_crate::module"),
        Some("src/main.rs"),
        Some(42),
    );
    assert_eq!(
        fields.get("_target").and_then(|v| v.as_str()),
        Some("my_crate::module")
    );
    assert_eq!(
        fields.get("_file").and_then(|v| v.as_str()),
        Some("src/main.rs")
    );
    assert_eq!(fields.get("_line").and_then(|v| v.as_u64()), Some(42));
}

#[test]
fn test_gelf_service_name_none() {
    let mut fields = Map::new();
    crate::gelf::add_service_field(&mut fields, None);
    assert!(!fields.contains_key("_service"));
}
