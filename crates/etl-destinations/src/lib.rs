//! ETL destination implementations.
//!
//! Provides implementations of the ETL destination trait for analytical
//! destinations that mirror a Postgres source: DuckLake for lakehouse storage
//! and Doris for MPP serving.

#[cfg(feature = "ducklake")]
mod retry;
#[cfg(any(feature = "doris", feature = "ducklake"))]
mod sql;

#[cfg(feature = "doris")]
pub mod doris;
#[cfg(feature = "ducklake")]
pub mod ducklake;
#[cfg(feature = "egress")]
pub mod egress;
