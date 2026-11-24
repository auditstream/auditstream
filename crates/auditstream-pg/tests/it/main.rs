//! Integration test binary for `auditstream-pg`.

#[test]
fn crate_loads() {
    assert_eq!(auditstream_pg::CRATE_NAME, "auditstream-pg");
    assert_eq!(auditstream_pg::core_crate_name(), "auditstream-core");
}
