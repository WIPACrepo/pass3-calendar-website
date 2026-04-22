pub mod importers;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{migrate::MigrateError, postgres::PgPool, Type};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertFileResult {
    Inserted,
    DuplicateSha512,
    ExistingStageFileMismatch,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NdJsonFileRecord {
    pub uuid: Uuid,
    #[serde(default)]
    pub logical_name: String,
    pub checksum: Checksum,
    pub processing_level: String,
    pub run: RunInfo,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Checksum {
    pub sha512: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RunInfo {
    pub run_number: i32,
    pub part_number: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Step1FileRecord {
    pub checksum: Checksum,
    pub logical_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "stage")]
pub enum Stage {
    #[sqlx(rename = "Raw Data")]
    RawData,
    #[sqlx(rename = "Step 1")]
    Step1,
    #[sqlx(rename = "Step 2")]
    Step2,
}

impl Stage {
    pub fn from_processing_level(level: &str) -> Option<Self> {
        match level {
            "PFRaw" => Some(Stage::RawData),
            _ => None,
        }
    }

    pub fn from_cli_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "raw" | "rawdata" | "raw-data" => Some(Stage::RawData),
            "step1" | "step-1" | "step_1" => Some(Stage::Step1),
            "step2" | "step-2" | "step_2" => Some(Stage::Step2),
            _ => None,
        }
    }
}

pub async fn run_app_migrations(pool: &PgPool) -> Result<(), MigrateError> {
    match sqlx::migrate!("./migrations").run(pool).await {
        Ok(()) => Ok(()),
        Err(MigrateError::VersionMismatch(version)) => {
            eprintln!(
                "Warning: migration {version} was previously applied with a different checksum. Continuing with the existing database schema. If this database should be managed by the current migration files, repair the _sqlx_migrations entry or use a fresh database."
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub async fn ensure_run_exists(pool: &PgPool, run_number: i32) -> Result<(), sqlx::Error> {
    let placeholder_time = DateTime::<Utc>::from_timestamp(0, 0)
        .expect("unix epoch should always be representable");

    sqlx::query(
        "INSERT INTO runs (run_number, run_start_date, run_end_date, state, url)
         VALUES ($1, $2, $3, $4::workflow_state, $5)
         ON CONFLICT (run_number) DO NOTHING"
    )
    .bind(run_number)
    .bind(placeholder_time)
    .bind(placeholder_time)
    .bind("Not Yet Started")
    .bind(format!("https://live.icecube.wisc.edu/run/{run_number}"))
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn insert_file(
    pool: &PgPool,
    id: Uuid,
    run_number: i32,
    part_number: i32,
    stage: Stage,
    file_path: &str,
    sha512: &str,
) -> Result<InsertFileResult, sqlx::Error> {
    ensure_run_exists(pool, run_number).await?;

    if matches!(stage, Stage::Step1 | Stage::Step2) {
        let existing_sha512 = sqlx::query_scalar::<_, String>(
            "SELECT sha512 FROM run_files WHERE run_number = $1 AND part_number = $2 AND stage = $3::stage LIMIT 1"
        )
        .bind(run_number)
        .bind(part_number)
        .bind(stage)
        .fetch_optional(pool)
        .await?;

        if let Some(existing_sha512) = existing_sha512 {
            return if existing_sha512 == sha512 {
                Ok(InsertFileResult::DuplicateSha512)
            } else {
                Ok(InsertFileResult::ExistingStageFileMismatch)
            };
        }
    }

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM run_files WHERE sha512 = $1)"
    )
    .bind(sha512)
    .fetch_one(pool)
    .await?;

    if exists {
        return Ok(InsertFileResult::DuplicateSha512);
    }

    sqlx::query(
        "INSERT INTO run_files (id, run_number, part_number, stage, file_path, sha512)
         VALUES ($1, $2, $3, $4::stage, $5, $6)
         ON CONFLICT (id) DO UPDATE 
            SET run_number = EXCLUDED.run_number,
                part_number = EXCLUDED.part_number,
                stage = EXCLUDED.stage,
                file_path = EXCLUDED.file_path,
                sha512 = EXCLUDED.sha512"
    )
    .bind(id)
    .bind(run_number)
    .bind(part_number)
    .bind(stage)
    .bind(file_path)
    .bind(sha512)
    .execute(pool)
    .await?;
    Ok(InsertFileResult::Inserted)
}
