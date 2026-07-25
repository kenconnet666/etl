mod commands;
mod utils;

use anyhow::Result;
use clap::{Parser, Subcommand};
use commands::{
    CheckArgs, ExampleArgs, FixArgs, FmtArgs, MigrateArgs, MsrvArgs, NextestArgs, PgFillTableArgs,
    PostgresArgs, SeedArgs, TestArgs, VendorDuckdbArgs,
};

#[derive(Parser)]
#[command(name = "xtask", about = "Project task runner")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pre-PR gate: fmt, sort, clippy.
    Check(CheckArgs),
    /// Run a destination example (e.g. `cargo x example ducklake`).
    Example(ExampleArgs),
    /// Auto-fix: clippy --fix, fmt, sort.
    Fix(FixArgs),
    /// Format code with nightly rustfmt.
    Fmt(FmtArgs),
    /// Run database migrations.
    Migrate(MigrateArgs),
    /// Verify MSRV consistency across Cargo.toml, rust-toolchain.toml, and
    /// cargo-msrv.
    Msrv(MsrvArgs),
    /// Run tests via nextest, sharded across multiple Postgres clusters.
    Nextest(NextestArgs),
    /// Fill one Postgres table to a target size using parallel COPY workers.
    #[command(name = "pg-fill-table")]
    PgFillTable(PgFillTableArgs),
    /// Manage test Postgres clusters.
    Postgres(PostgresArgs),
    /// Seed a Postgres database with test tables and data for destination
    /// examples.
    Seed(SeedArgs),
    /// Run local tests via nextest.
    Test(TestArgs),
    /// Download and vendor DuckDB extensions.
    #[command(name = "vendor-duckdb")]
    VendorDuckdb(VendorDuckdbArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Check(cmd) => cmd.run(),
        Command::Example(cmd) => cmd.run(),
        Command::Fix(cmd) => cmd.run(),
        Command::Fmt(cmd) => cmd.run(),
        Command::Migrate(cmd) => cmd.run(),
        Command::Msrv(cmd) => cmd.run(),
        Command::Nextest(cmd) => cmd.run(),
        Command::PgFillTable(cmd) => cmd.run(),
        Command::Postgres(cmd) => cmd.run(),
        Command::Seed(cmd) => cmd.run(),
        Command::Test(cmd) => cmd.run(),
        Command::VendorDuckdb(cmd) => cmd.run(),
    }
}
