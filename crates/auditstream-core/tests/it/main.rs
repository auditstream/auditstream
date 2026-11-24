//! Integration test binary for `auditstream-core`.

#[test]
fn crate_loads() {
    assert_eq!(auditstream_core::CRATE_NAME, "auditstream-core");
}
