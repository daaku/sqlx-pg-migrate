//! A library to run migrations on a PostgreSQL database using SQLx.
//!
//! Make a directory that contains your migrations. The library will run thru
//! all the files in sorted order. The suggested naming convention is
//! `000_first.sql`, `001_second.sql` and so on.
//!
//! The library:
//! 1. Will create the DB if necessary.
//! 1. Will either create a table called `sqlx_pg_migrate` or the given table name
//!    to manage the migration state.
//! 1. Will run everything in a single transaction, so all pending migrations
//!    are run, or nothing.
//! 1. Expects you to never delete or rename a migration.
//! 1. Expects you to not put a new migration between two existing ones.
//! 1. Expects file names and contents to be UTF-8.
//! 1. There are no rollbacks - just write a new migration.
//!
//! You'll need to add these two crates as dependencies:
//! ```toml
//! [dependencies]
//! include_dir = "0.6"
//! sqlx-pg-migrate = "1.0"
//! ```
//!
//! Note! If you're using tokio, add `runtime-tokio` as a feature to the `sqlx` dependency:
//! ```toml
//! sqlx = { version = "0.3", default-features = false, features = ["postgres", "runtime-tokio"] }
//! ```
//!
//! The usage looks like this:
//!
//! ```
//! use sqlx_pg_migrate::migrate;
//! use include_dir::{include_dir, Dir};
//!
//! // Use include_dir! to include your migrations into your binary.
//! // The path here is relative to your cargo root.
//! static MIGRATIONS: Dir = include_dir!("migrations");
//!
//! # #[async_attributes::main]
//! # async fn main() -> std::result::Result<(), sqlx_pg_migrate::Error> {
//! #    let db_url = std::env::var("DATABASE_URL")
//! #        .unwrap_or(String::from("postgresql://localhost/sqlxpgmigrate_doctest"));
//! // Somewhere, probably in main, call the migrate function with your DB URL
//! // and the included migrations.
//! migrate(&db_url, &MIGRATIONS, None).await?;
//! #    Ok(())
//! # }
//! ```

use include_dir::Dir;
use sqlx::postgres::PgRow;
use sqlx::{Connect, Connection, Executor, PgConnection, Row};
use thiserror::Error;

/// The various kinds of errors that can arise when running the migrations.
#[derive(Error, Debug)]
pub enum Error {
    #[error("expected migration `{0}` to already have been run")]
    MissingMigration(String),

    #[error("invalid URL `{0}`: could not determine DB name")]
    InvalidURL(String),

    #[error("error connecting to existing database: {}", .source)]
    ExistingConnectError { source: sqlx::Error },

    #[error("error connecting to base URL `{}` to create DB: {}", .url, .source)]
    BaseConnect { url: String, source: sqlx::Error },

    #[error("error finding current migrations: {}", .source)]
    CurrentMigrations { source: sqlx::Error },

    #[error("invalid utf-8 bytes in migration content: {0}")]
    InvalidMigrationContent(std::path::PathBuf),

    #[error("invalid utf-8 bytes in migration path: {0}")]
    InvalidMigrationPath(std::path::PathBuf),

    #[error("more migrations run than are known indicating possibly deleted migrations")]
    DeletedMigrations,

    #[error(transparent)]
    DB(#[from] sqlx::Error),
}

type Result<T> = std::result::Result<T, Error>;

fn base_and_db(url: &str) -> Result<(&str, &str)> {
    let base_split: Vec<&str> = url.rsplitn(2, '/').collect();
    if base_split.len() != 2 {
        return Err(Error::InvalidURL(url.to_string()));
    }
    let qmark_split: Vec<&str> = base_split[0].splitn(2, '?').collect();
    Ok((base_split[1], qmark_split[0]))
}

async fn maybe_make_db(url: &str) -> Result<()> {
    match PgConnection::connect(url).await {
        Ok(_) => return Ok(()), // it exists, we're done
        Err(err) => {
            if let sqlx::Error::Database(dberr) = err {
                // this indicates the database doesn't exist
                if let Some("3D000") = dberr.code() {
                    Ok(()) // it doesn't exist, continue to create it
                } else {
                    Err(Error::ExistingConnectError {
                        source: sqlx::Error::Database(dberr),
                    })
                }
            } else {
                Err(Error::ExistingConnectError { source: err })
            }
        }
    }?;

    let (base_url, db_name) = base_and_db(url)?;
    let mut db = match PgConnection::connect(&format!("{}/postgres", base_url)).await {
        Ok(db) => db,
        Err(err) => {
            return Err(Error::BaseConnect {
                url: base_url.to_string(),
                source: err,
            });
        }
    };
    sqlx::query(&format!(r#"CREATE DATABASE "{}""#, db_name))
        .execute(&mut db)
        .await?;
    Ok(())
}

async fn get_migrated(db: &mut PgConnection, migration_table: &str) -> Result<Vec<String>> {
    let migrated = sqlx::query(&format!("SELECT migration FROM {} ORDER BY id", migration_table))
        .bind(migration_table)
        .try_map(|row: PgRow| row.try_get("migration"))
        .fetch_all(db)
        .await;
    match migrated {
        Ok(migrated) => Ok(migrated),
        Err(err) => {
            if let sqlx::Error::Database(dberr) = err {
                // this indicates the table doesn't exist
                if let Some("42P01") = dberr.code() {
                    Ok(vec![])
                } else {
                    Err(Error::CurrentMigrations {
                        source: sqlx::Error::Database(dberr),
                    })
                }
            } else {
                Err(Error::CurrentMigrations { source: err })
            }
        }
    }
}

const DEFAULT_MIGRATION_TABLE: &str = "sqlx_pg_migrate";

/// Runs the migrations contained in the directory. See module documentation for
/// more information.
pub async fn migrate(url: &str, dir: &Dir<'_>, table: Option<&str>) -> Result<()> {
    let migration_table = table.unwrap_or_else(|| DEFAULT_MIGRATION_TABLE);

    maybe_make_db(url).await?;
    let mut db = PgConnection::connect(url).await?;
    let migrated = get_migrated(&mut db, migration_table).await?;
    let mut tx = db.begin().await?;
    if migrated.is_empty() {
        sqlx::query(
            &format!(r#"
                CREATE TABLE IF NOT EXISTS {} (
                    id SERIAL PRIMARY KEY,
                    migration TEXT UNIQUE,
                    created TIMESTAMP NOT NULL DEFAULT current_timestamp
                );
            "#, migration_table)
        )
            .execute(&mut tx)
            .await?;
    }
    let mut files: Vec<_> = dir.files().iter().collect();
    if migrated.len() > files.len() {
        return Err(Error::DeletedMigrations);
    }
    files.sort_by(|a, b| a.path().partial_cmp(b.path()).unwrap());
    for (pos, f) in files.iter().enumerate() {
        let path = f
            .path()
            .to_str()
            .ok_or_else(|| Error::InvalidMigrationPath(f.path().to_owned()))?;

        if pos < migrated.len() {
            if migrated[pos] != path {
                return Err(Error::MissingMigration(path.to_owned()));
            }
            continue;
        }

        let content = f
            .contents_utf8()
            .ok_or_else(|| Error::InvalidMigrationContent(f.path().to_owned()))?;
        tx.execute(content).await?;
        sqlx::query(&format!("INSERT INTO {} (migration) VALUES ($1)", migration_table))
            .bind(path)
            .execute(&mut tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::migrate;
    use include_dir::{include_dir, Dir};

    static MIGRATIONS: Dir = include_dir!("migrations");

    #[async_attributes::test]
    async fn it_works() -> std::result::Result<(), super::Error> {
        let url = std::env::var("DATABASE_URL").unwrap_or(String::from(
            "postgresql://localhost/sqlxpgmigrate1?sslmode=disable",
        ));
        // run it twice, second time should be a no-op
        for _ in 0..2 {
            match migrate(&url, &MIGRATIONS, None).await {
                Err(err) => panic!("migrate failed with: {}", err),
                _ => (),
            };
        }
        Ok(())
    }
}
