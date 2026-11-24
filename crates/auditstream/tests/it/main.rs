//! Integration test binary for the `auditstream` CLI crate.

mod cli;
mod init;
mod status;
mod support;
mod verify;

#[test]
fn binary_crate_compiles() {
    assert_eq!(auditstream_core::CRATE_NAME, "auditstream-core");
}
