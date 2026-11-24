//! Workspace automation task runner (`cargo xtask`).

fn main() {
    println!("auditstream xtask");
    println!();
    println!("Planned subcommands:");
    println!("  ci       Run all local quality gates (fmt, clippy, tests, deny, docs)");
    println!("  codegen  Regenerate schemas and artifacts");
    println!("  bench    Run pgbench capture benchmarks");
}
