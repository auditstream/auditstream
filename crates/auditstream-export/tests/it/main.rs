//! Integration test binary for `auditstream-export`.

#[test]
fn crate_loads() {
    assert_eq!(auditstream_export::CRATE_NAME, "auditstream-export");
    assert_eq!(auditstream_export::core_crate_name(), "auditstream-core");
}
