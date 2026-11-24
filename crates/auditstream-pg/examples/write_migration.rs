//! Example binary exposing D4's migration writer for JC3 and JD5 before `auditstream init` exists.
//!
//! Usage:
//! ```text
//! cargo run -p auditstream-pg --example write_migration -- \
//!   --format prisma|drizzle|sql \
//!   --out <dir> \
//!   [--schema auditstream] \
//!   [--app-role app_user]... \
//!   [--table public.invoice:id:password_hash]... \
//!   [--drizzle-layout 0.45|1.x] \
//!   [--now 2026-11-24T12:00:00Z]
//! ```

use std::env;
use std::path::PathBuf;
use std::process;

use auditstream_pg::ddl::{DEFAULT_SCHEMA, DdlOptions};
use auditstream_pg::migrations::{
    self, DrizzleLayout, MigrationFormat, MigrationOptions, WriteOutcome,
};
use auditstream_pg::triggers::TableTriggerSpec;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

fn parse_table_spec(raw: &str) -> Result<TableTriggerSpec, String> {
    let mut parts = raw.split(':');
    let qualified = parts
        .next()
        .ok_or_else(|| format!("invalid --table value `{raw}`"))?;
    let (schema, table) = qualified.split_once('.').ok_or_else(|| {
        format!("--table must be `schema.table[:pk1,pk2[:excl1,excl2]]`, got `{raw}`")
    })?;
    let pk: Vec<String> = parts
        .next()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let excluded: Vec<String> = parts
        .next()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    Ok(TableTriggerSpec::new(schema, table, pk, excluded))
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let mut format: Option<MigrationFormat> = None;
    let mut out_dir: Option<PathBuf> = None;
    let mut schema = DEFAULT_SCHEMA.to_owned();
    let mut app_roles: Vec<String> = Vec::new();
    let mut tables: Vec<TableTriggerSpec> = Vec::new();
    let mut drizzle_layout: Option<DrizzleLayout> = None;
    let mut now = OffsetDateTime::now_utc();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--format" => {
                let val = args.next().ok_or("--format requires a value")?;
                format = Some(match val.as_str() {
                    "prisma" => MigrationFormat::Prisma,
                    "drizzle" => MigrationFormat::Drizzle,
                    "sql" => MigrationFormat::Sql,
                    other => {
                        return Err(format!(
                            "unknown --format `{other}` (use prisma|drizzle|sql)"
                        ));
                    }
                });
            }
            "--out" => {
                out_dir = Some(PathBuf::from(args.next().ok_or("--out requires a path")?));
            }
            "--schema" => {
                schema = args.next().ok_or("--schema requires a value")?;
            }
            "--app-role" => {
                app_roles.push(args.next().ok_or("--app-role requires a role name")?);
            }
            "--table" => {
                let raw = args.next().ok_or("--table requires a value")?;
                tables.push(parse_table_spec(&raw)?);
            }
            "--drizzle-layout" => {
                let val = args.next().ok_or("--drizzle-layout requires 0.45 or 1.x")?;
                drizzle_layout = Some(match val.as_str() {
                    "0.45" => DrizzleLayout::V045,
                    "1.x" => DrizzleLayout::V1,
                    other => {
                        return Err(format!("unknown --drizzle-layout `{other}` (use 0.45|1.x)"));
                    }
                });
            }
            "--now" => {
                let val = args.next().ok_or("--now requires an RFC 3339 timestamp")?;
                now = OffsetDateTime::parse(&val, &Rfc3339)
                    .map_err(|err| format!("invalid --now `{val}`: {err}"))?;
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }

    let format = format.ok_or("missing required --format prisma|drizzle|sql")?;
    let out_dir = out_dir.ok_or("missing required --out <dir>")?;

    let mut opts = MigrationOptions::new(
        format,
        out_dir,
        now,
        DdlOptions::new(schema, app_roles),
        tables,
    );
    if let Some(layout) = drizzle_layout {
        opts = opts.with_drizzle_layout(layout);
    }

    match migrations::write(&opts).map_err(|err| err.to_string())? {
        WriteOutcome::Written {
            files,
            dropped_tables,
        } => {
            for path in files {
                println!("{}", path.display());
            }
            if !dropped_tables.is_empty() {
                eprintln!("dropped triggers for: {dropped_tables:?}");
            }
        }
        WriteOutcome::UpToDate { existing } => {
            eprintln!("up to date: {}", existing.display());
        }
    }

    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        process::exit(2);
    }
}
