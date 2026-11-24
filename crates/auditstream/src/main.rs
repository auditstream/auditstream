//! CLI entry point for Auditstream.

use anyhow as _;
use clap as _;
use tokio as _;

fn main() {
    let _ = (
        auditstream_core::CRATE_NAME,
        auditstream_pg::CRATE_NAME,
        auditstream_export::CRATE_NAME,
    );
}
