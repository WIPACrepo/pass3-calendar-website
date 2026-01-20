use serde::{Deserialize, Serialize};
use sqlx::{postgres::PgPool, Type};
use uuid::Uuid;

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
}

pub async fn insert_file(
    pool: &PgPool,
    id: Uuid,
    run_number: i32,
    part_number: i32,
    stage: Stage,
    file_path: &str,
    sha512: &str,
) -> Result<(), sqlx::Error> {
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
    Ok(())
}
