use crate::{
    ensure_run_exists, insert_file, InsertFileResult, NdJsonFileRecord, Stage, Step1FileRecord,
};
use chrono::{DateTime, NaiveDateTime, Utc};
use ndarray::{ArrayD, IxDyn, OwnedRepr};
use ndarray_npy::NpzReader;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};
use sqlx::postgres::PgPool;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use uuid::Uuid;

type DynError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Default, Clone, Copy)]
pub struct ImportReport {
    pub imported: usize,
    pub skipped: usize,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
struct ChargeDistributionRecord {
    run_number: i32,
    atwd_histograms: Value,
    fadc_histograms: Value,
    llh_comparison: Value,
}

impl ImportReport {
    fn imported(&mut self) {
        self.imported += 1;
    }

    fn skipped(&mut self) {
        self.skipped += 1;
    }
}

#[derive(Debug, Deserialize)]
struct GrlFile {
    runs: Vec<GrlRunRecord>,
}

#[derive(Debug, Deserialize)]
struct GrlRunRecord {
    good_i3: bool,
    #[serde(deserialize_with = "deserialize_grl_timestamp_field")]
    good_tstart: Option<String>,
    #[serde(deserialize_with = "deserialize_grl_timestamp_field")]
    good_tstop: Option<String>,
    run: i32,
}

#[derive(Debug, Deserialize)]
struct FilterRateEnvelope {
    filter_rates: Value,
}

pub fn read_path_list(list_file: &Path) -> Result<Vec<PathBuf>, DynError> {
    let file = File::open(list_file)?;
    let reader = BufReader::new(file);
    let mut paths = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        paths.push(PathBuf::from(trimmed));
    }

    Ok(paths)
}

pub fn list_matching_files<F>(input: &Path, predicate: F) -> Result<Vec<PathBuf>, DynError>
where
    F: Fn(&Path) -> bool,
{
    if input.is_file() {
        return Ok(if predicate(input) {
            vec![input.to_path_buf()]
        } else {
            Vec::new()
        });
    }

    let mut files = Vec::new();
    for entry in std::fs::read_dir(input)? {
        let entry = entry?;
        let path = entry.path();
        if predicate(&path) {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

pub fn is_gcd_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    let filename = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    filename.contains("GCD") && filename.ends_with(".i3.zst")
}

pub fn is_filter_rate_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.ends_with(".filter_rates.txt"))
        .unwrap_or(false)
}

pub fn is_charge_distribution_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.ends_with(".npz"))
        .unwrap_or(false)
}

pub fn is_charge_comparison_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    path.extension().and_then(|ext| ext.to_str()) == Some("json")
}

pub fn extract_run_number(path: &Path) -> Option<i32> {
    let filename = path.file_name()?.to_str()?;
    extract_run_number_from_str(filename)
}

pub fn extract_run_number_from_str(value: &str) -> Option<i32> {
    let re = Regex::new(r"Run(\d+)").ok()?;
    let captures = re.captures(value)?;
    captures.get(1)?.as_str().parse::<i32>().ok()
}

pub fn canonical_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

pub fn compute_sha512(path: &Path) -> Result<String, std::io::Error> {
    use sha2::{Digest, Sha512};

    let mut file = File::open(path)?;
    let mut hasher = Sha512::new();
    std::io::copy(&mut file, &mut hasher)?;
    let hash = hasher.finalize();
    Ok(format!("{:x}", hash))
}

pub async fn import_pfraw_file(pool: &PgPool, pfraw_path: &Path) -> Result<ImportReport, DynError> {
    let file = File::open(pfraw_path)?;
    let reader = BufReader::new(file);
    let mut report = ImportReport::default();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() || line.contains("\"file_count\"") {
            continue;
        }

        let record: NdJsonFileRecord = match serde_json::from_str(&line) {
            Ok(record) => record,
            Err(error) => {
                println!("Skipping line {}: Parse error: {}", line_num + 1, error);
                report.skipped();
                continue;
            }
        };

        let stage = match Stage::from_processing_level(&record.processing_level) {
            Some(stage) => stage,
            None => {
                println!(
                    "Skipping line {}: Unknown processing level '{}'",
                    line_num + 1,
                    record.processing_level
                );
                report.skipped();
                continue;
            }
        };

        let file_path = Path::new(&record.logical_name)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&record.logical_name)
            .to_string();

        match insert_file(
            pool,
            record.uuid,
            record.run.run_number,
            record.run.part_number,
            stage,
            &file_path,
            &record.checksum.sha512,
        )
        .await
        {
            Ok(InsertFileResult::Inserted) => report.imported(),
            Ok(InsertFileResult::DuplicateSha512) => {
                println!(
                    "Skipping run {} part {}: file with SHA512 {} already exists",
                    record.run.run_number,
                    record.run.part_number,
                    record.checksum.sha512
                );
                report.skipped();
            }
            Err(error) => {
                println!("Error importing run {}: {}", record.run.run_number, error);
                report.skipped();
            }
        }
    }

    Ok(report)
}

pub async fn import_step1_file(pool: &PgPool, step1_path: &Path) -> Result<ImportReport, DynError> {
    let content = std::fs::read_to_string(step1_path)?;
    let data: HashMap<String, Vec<Step1FileRecord>> = serde_json::from_str(&content)?;
    let re = Regex::new(r"Run(\d+)_Subrun\d+_(\d+)\.")?;
    let mut report = ImportReport::default();

    for (_key, records) in data {
        for record in records {
            let file_path = Path::new(&record.logical_name)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&record.logical_name)
                .to_string();

            let (run_number, part_number) = if let Some(captures) = re.captures(&file_path) {
                let run_number = captures[1].parse::<i32>().unwrap_or(0);
                let part_number = captures[2].parse::<i32>().unwrap_or(0);
                (run_number, part_number)
            } else {
                println!("Skipping file '{}': Could not parse run/part number", file_path);
                report.skipped();
                continue;
            };

            let id = Uuid::new_v5(&Uuid::NAMESPACE_URL, record.checksum.sha512.as_bytes());

            match insert_file(
                pool,
                id,
                run_number,
                part_number,
                Stage::Step1,
                &file_path,
                &record.checksum.sha512,
            )
            .await
            {
                Ok(InsertFileResult::Inserted) => report.imported(),
                Ok(InsertFileResult::DuplicateSha512) => {
                    println!(
                        "Skipping run {} part {}: file with SHA512 {} already exists",
                        run_number,
                        part_number,
                        record.checksum.sha512
                    );
                    report.skipped();
                }
                Err(error) => {
                    println!("Error importing run {}: {}", run_number, error);
                    report.skipped();
                }
            }
        }
    }

    Ok(report)
}

pub async fn import_grl_file(pool: &PgPool, grl_path: &Path) -> Result<ImportReport, DynError> {
    let content = std::fs::read_to_string(grl_path)?;
    let grl: GrlFile = serde_json::from_str(&content)?;
    let mut report = ImportReport::default();

    for record in grl.runs {
        if !record.good_i3 {
            report.skipped();
            continue;
        }

        let run_start_date = parse_grl_timestamp(record.good_tstart.as_deref())?;
        let run_end_date = parse_grl_timestamp(record.good_tstop.as_deref())?;

        match sqlx::query(
            "INSERT INTO runs (run_number, run_start_date, run_end_date, state, url)
             VALUES ($1, $2, $3, $4::workflow_state, $5)
             ON CONFLICT (run_number) DO UPDATE
             SET run_start_date = EXCLUDED.run_start_date,
                 run_end_date = EXCLUDED.run_end_date,
                 url = EXCLUDED.url"
        )
        .bind(record.run)
        .bind(run_start_date)
        .bind(run_end_date)
        .bind("Not Yet Started")
        .bind(format!("https://live.icecube.wisc.edu/run/{}", record.run))
        .execute(pool)
        .await
        {
            Ok(_) => report.imported(),
            Err(error) => {
                println!("Error importing GRL run {}: {}", record.run, error);
                report.skipped();
            }
        }
    }

    Ok(report)
}

pub fn validate_grl_json_file(grl_path: &Path) -> Result<(), DynError> {
    let content = std::fs::read_to_string(grl_path)?;
    let grl: GrlFile = serde_json::from_str(&content)?;

    let total = grl.runs.len();
    let valid = grl.runs.iter().filter(|record| record.good_i3).count();
    let skipped = total.saturating_sub(valid);

    println!("\n=== GRL Validation Report ===");
    println!("Valid runs (good_i3 = true): {}", valid);
    println!("Skipped runs: {}", skipped);
    println!("Total processed: {}", total);
    println!("\n✓ GRL JSON is valid and ready for import!");
    println!(
        "Run the following to import: cargo run --bin setup_db -- --grl-json {}",
        grl_path.display()
    );

    Ok(())
}

pub async fn import_gcd_files(
    pool: &PgPool,
    stage: Stage,
    paths: &[PathBuf],
) -> Result<ImportReport, DynError> {
    let mut report = ImportReport::default();

    for path in paths {
        let Some(run_number) = extract_run_number(path) else {
            println!("Skipping file '{}': Could not parse run number", path.display());
            report.skipped();
            continue;
        };

        let sha512 = match compute_sha512(path) {
            Ok(sha512) => sha512,
            Err(error) => {
                println!("Error computing SHA512 for '{}': {}", path.display(), error);
                report.skipped();
                continue;
            }
        };

        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM gcd_files WHERE sha512 = $1)"
        )
        .bind(&sha512)
        .fetch_one(pool)
        .await
        .unwrap_or(false);

        if exists {
            println!(
                "Skipping file '{}': GCD file with same SHA512 already exists",
                path.display()
            );
            report.skipped();
            continue;
        }

        let location = canonical_path(path);
        if let Err(error) = insert_gcd_file(pool, run_number, stage, &location, &sha512).await {
            println!("Error importing GCD file for run {}: {}", run_number, error);
            report.skipped();
        } else {
            report.imported();
        }
    }

    Ok(report)
}

pub async fn import_filter_rate_files(
    pool: &PgPool,
    stage: Stage,
    paths: &[PathBuf],
) -> Result<ImportReport, DynError> {
    let mut report = ImportReport::default();

    for path in paths {
        let Some(run_number) = extract_run_number(path) else {
            println!("Skipping file '{}': Could not parse run number", path.display());
            report.skipped();
            continue;
        };

        let payload: FilterRateEnvelope = match serde_json::from_str(&std::fs::read_to_string(path)?) {
            Ok(payload) => payload,
            Err(error) => {
                println!("Skipping file '{}': {}", path.display(), error);
                report.skipped();
                continue;
            }
        };

        if let Err(error) = upsert_filter_rates(pool, run_number, stage, &payload.filter_rates).await {
            println!("Error importing filter rates for run {}: {}", run_number, error);
            report.skipped();
        } else {
            report.imported();
        }
    }

    Ok(report)
}

pub async fn import_charge_distribution_files(
    pool: &PgPool,
    stage: Stage,
    paths: &[PathBuf],
) -> Result<ImportReport, DynError> {
    let mut report = ImportReport::default();

    for path in paths {
        let Some(run_number) = extract_run_number(path) else {
            println!("Skipping file '{}': Could not parse run number", path.display());
            report.skipped();
            continue;
        };

        let (atwd_histograms, fadc_histograms) = match load_charge_histograms(path) {
            Ok(histograms) => histograms,
            Err(error) => {
                println!("Error parsing charge distributions '{}': {}", path.display(), error);
                report.skipped();
                continue;
            }
        };

        if let Err(error) = upsert_charge_distributions(
            pool,
            run_number,
            stage,
            &atwd_histograms,
            &fadc_histograms,
        )
        .await
        {
            println!("Error importing charge distributions for run {}: {}", run_number, error);
            report.skipped();
        } else {
            report.imported();
        }
    }

    Ok(report)
}

pub async fn import_charge_comparison_files(
    pool: &PgPool,
    stage: Stage,
    paths: &[PathBuf],
) -> Result<ImportReport, DynError> {
    let mut report = ImportReport::default();

    for path in paths {
        let comparison: Value = match serde_json::from_str(&std::fs::read_to_string(path)?) {
            Ok(comparison) => comparison,
            Err(error) => {
                println!("Skipping comparison file '{}': {}", path.display(), error);
                report.skipped();
                continue;
            }
        };

        let run_number = comparison
            .get("data_file")
            .and_then(Value::as_str)
            .and_then(extract_run_number_from_str)
            .or_else(|| extract_run_number(path));

        let Some(run_number) = run_number else {
            println!(
                "Skipping comparison file '{}': Could not parse run number",
                path.display()
            );
            report.skipped();
            continue;
        };

        match upsert_charge_comparison(pool, run_number, stage, &comparison).await {
            Ok(()) => report.imported(),
            Err(sqlx::Error::RowNotFound) => {
                println!(
                    "Skipping comparison for run {}: charge histograms must be imported first",
                    run_number
                );
                report.skipped();
            }
            Err(error) => {
                println!("Error importing charge comparison for run {}: {}", run_number, error);
                report.skipped();
            }
        }
    }

    Ok(report)
}

pub async fn inspect_charge_distribution(
    pool: &PgPool,
    run_number: i32,
    stage: Stage,
    full_json: bool,
) -> Result<Option<Value>, DynError> {
    let record = sqlx::query_as::<_, ChargeDistributionRecord>(
        "SELECT run_number, atwd_histograms, fadc_histograms, llh_comparison
         FROM charge_distributions
         WHERE run_number = $1 AND stage = $2::stage"
    )
    .bind(run_number)
    .bind(stage)
    .fetch_optional(pool)
    .await?;

    Ok(record.map(|record| {
        if full_json {
            json!({
                "run_number": record.run_number,
                "stage": stage_label(stage),
                "atwd_histograms": record.atwd_histograms,
                "fadc_histograms": record.fadc_histograms,
                "llh_comparison": record.llh_comparison,
            })
        } else {
            json!({
                "run_number": record.run_number,
                "stage": stage_label(stage),
                "atwd_histograms": summarize_json_shape(&record.atwd_histograms),
                "fadc_histograms": summarize_json_shape(&record.fadc_histograms),
                "llh_comparison": summarize_json_shape(&record.llh_comparison),
            })
        }
    }))
}

async fn insert_gcd_file(
    pool: &PgPool,
    run_number: i32,
    stage: Stage,
    location: &str,
    sha512: &str,
) -> Result<(), sqlx::Error> {
    ensure_run_exists(pool, run_number).await?;

    sqlx::query(
        "INSERT INTO gcd_files (id, run_number, stage, location, sha512)
         VALUES ($1, $2, $3::stage, $4, $5)
         ON CONFLICT (run_number, stage) DO UPDATE
         SET location = EXCLUDED.location,
             sha512 = EXCLUDED.sha512,
             updated_at = CURRENT_TIMESTAMP"
    )
    .bind(Uuid::new_v4())
    .bind(run_number)
    .bind(stage)
    .bind(location)
    .bind(sha512)
    .execute(pool)
    .await?;

    Ok(())
}

async fn upsert_filter_rates(
    pool: &PgPool,
    run_number: i32,
    stage: Stage,
    filter_rates: &Value,
) -> Result<(), sqlx::Error> {
    ensure_run_exists(pool, run_number).await?;

    sqlx::query(
        "INSERT INTO filter_rates (id, run_number, stage, filter_rates)
         VALUES ($1, $2, $3::stage, $4)
         ON CONFLICT (run_number, stage) DO UPDATE
         SET filter_rates = EXCLUDED.filter_rates,
             updated_at = CURRENT_TIMESTAMP"
    )
    .bind(Uuid::new_v4())
    .bind(run_number)
    .bind(stage)
    .bind(filter_rates)
    .execute(pool)
    .await?;

    Ok(())
}

async fn upsert_charge_distributions(
    pool: &PgPool,
    run_number: i32,
    stage: Stage,
    atwd_histograms: &Value,
    fadc_histograms: &Value,
) -> Result<(), sqlx::Error> {
    ensure_run_exists(pool, run_number).await?;

    sqlx::query(
        "INSERT INTO charge_distributions (id, run_number, stage, atwd_histograms, fadc_histograms, llh_comparison)
         VALUES ($1, $2, $3::stage, $4, $5, $6)
         ON CONFLICT (run_number, stage) DO UPDATE
         SET atwd_histograms = EXCLUDED.atwd_histograms,
             fadc_histograms = EXCLUDED.fadc_histograms,
             updated_at = CURRENT_TIMESTAMP"
    )
    .bind(Uuid::new_v4())
    .bind(run_number)
    .bind(stage)
    .bind(atwd_histograms)
    .bind(fadc_histograms)
    .bind(Value::Object(Map::new()))
    .execute(pool)
    .await?;

    Ok(())
}

async fn upsert_charge_comparison(
    pool: &PgPool,
    run_number: i32,
    stage: Stage,
    comparison: &Value,
) -> Result<(), sqlx::Error> {
    let result = sqlx::query(
        "UPDATE charge_distributions
         SET llh_comparison = $3,
             updated_at = CURRENT_TIMESTAMP
         WHERE run_number = $1
           AND stage = $2::stage
           AND atwd_histograms <> '{}'::jsonb
           AND fadc_histograms <> '{}'::jsonb"
    )
    .bind(run_number)
    .bind(stage)
    .bind(comparison)
    .execute(pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(sqlx::Error::RowNotFound);
    }

    Ok(())
}

fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::RawData => "Raw Data",
        Stage::Step1 => "Step 1",
        Stage::Step2 => "Step 2",
    }
}

fn summarize_json_shape(value: &Value) -> Value {
    if let Some(object) = value.as_object() {
        if let (Some(dtype), Some(shape)) = (object.get("dtype"), object.get("shape")) {
            return json!({
                "dtype": dtype,
                "shape": shape,
            });
        }

        let mut summary = Map::new();
        for (key, nested) in object {
            summary.insert(key.clone(), summarize_json_shape(nested));
        }
        return Value::Object(summary);
    }

    value.clone()
}

fn deserialize_grl_timestamp_field<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;

    Ok(value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
            None
        } else {
            Some(trimmed.to_string())
        }
    }))
}

fn parse_grl_timestamp(value: Option<&str>) -> Result<DateTime<Utc>, DynError> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(unix_epoch());
    };

    if value.eq_ignore_ascii_case("null") {
        return Ok(unix_epoch());
    }

    let normalized = normalize_grl_timestamp(value, 6);

    if let Ok(parsed) = DateTime::parse_from_rfc3339(&normalized) {
        return Ok(parsed.with_timezone(&Utc));
    }

    if let Ok(parsed) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%d %H:%M:%S%.f") {
        return Ok(parsed.and_utc());
    }

    if let Ok(parsed) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%d %H:%M:%S") {
        return Ok(parsed.and_utc());
    }

    Err(format!("unsupported GRL timestamp '{value}'").into())
}

fn normalize_grl_timestamp(value: &str, fractional_digits: usize) -> String {
    let Some((prefix, suffix)) = value.split_once('.') else {
        return value.to_string();
    };

    let fractional_len = suffix
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();

    if fractional_len == 0 {
        return value.to_string();
    }

    let (fractional, rest) = suffix.split_at(fractional_len);
    let truncated: String = fractional.chars().take(fractional_digits).collect();

    format!("{prefix}.{truncated}{rest}")
}

fn unix_epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).expect("unix epoch should always be representable")
}

fn load_charge_histograms(path: &Path) -> Result<(Value, Value), DynError> {
    let file = File::open(path)?;
    let mut npz = NpzReader::new(file)?;
    let bounds = npz.by_name::<OwnedRepr<f64>, IxDyn>("bounds.npy")?;
    let bins = npz.by_name::<OwnedRepr<f64>, IxDyn>("bins.npy")?;
    let atwd = npz.by_name::<OwnedRepr<f32>, IxDyn>("atwd.npy")?;
    let atwd_mean = npz.by_name::<OwnedRepr<f64>, IxDyn>("atwd_mean.npy")?;
    let atwd_sigma = npz.by_name::<OwnedRepr<f64>, IxDyn>("atwd_sigma.npy")?;
    let fadc = npz.by_name::<OwnedRepr<f32>, IxDyn>("fadc.npy")?;
    let fadc_mean = npz.by_name::<OwnedRepr<f64>, IxDyn>("fadc_mean.npy")?;
    let fadc_sigma = npz.by_name::<OwnedRepr<f64>, IxDyn>("fadc_sigma.npy")?;
    let allow_pickle = npz.by_name::<OwnedRepr<bool>, IxDyn>("allow_pickle.npy")?;

    let start = read_npz_datetime_scalar(&mut npz, "start.npy")
        .or_else(|_| read_npz_string_scalar(&mut npz, "start.npy"))?;

    Ok((
        json!({
            "bounds": array_to_json("f64", bounds.clone()),
            "bins": array_to_json("f64", bins.clone()),
            "start": start,
            "allow_pickle": array_to_json("bool", allow_pickle.clone()),
            "histogram": array_to_json("f32", atwd),
            "mean": array_to_json("f64", atwd_mean),
            "sigma": array_to_json("f64", atwd_sigma),
        }),
        json!({
            "bounds": array_to_json("f64", bounds.clone()),
            "bins": array_to_json("f64", bins.clone()),
            "start": start.clone(),
            "allow_pickle": array_to_json("bool", allow_pickle.clone()),
            "histogram": array_to_json("f32", fadc),
            "mean": array_to_json("f64", fadc_mean),
            "sigma": array_to_json("f64", fadc_sigma),
        }),
    ))
}

fn array_to_json<T>(dtype: &str, array: ArrayD<T>) -> Value
where
    T: Serialize + Clone,
{
    json!({
        "dtype": dtype,
        "shape": array.shape(),
        "values": array.iter().cloned().collect::<Vec<T>>(),
    })
}

fn read_npz_datetime_scalar(npz: &mut NpzReader<File>, name: &str) -> Result<Value, DynError> {
    let bytes = npz.by_name::<OwnedRepr<i64>, IxDyn>(name)?;
    let first = bytes
        .iter()
        .next()
        .copied()
        .ok_or_else(|| format!("datetime array '{name}' is empty"))?;

    let seconds = first.div_euclid(1_000_000);
    let micros = first.rem_euclid(1_000_000) as u32;
    let nanos = micros.saturating_mul(1_000);
    let timestamp = DateTime::<Utc>::from_timestamp(seconds, nanos)
        .ok_or_else(|| format!("invalid datetime micros in '{name}'"))?;

    Ok(Value::String(timestamp.to_rfc3339()))
}

fn read_npz_string_scalar(npz: &mut NpzReader<File>, name: &str) -> Result<Value, DynError> {
    let chars = npz.by_name::<OwnedRepr<u8>, IxDyn>(name)?;
    let value = String::from_utf8(chars.iter().copied().collect())?;
    Ok(Value::String(value.trim_end_matches('\0').to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_grl_timestamp_truncates_subsecond_precision() {
        let parsed = parse_grl_timestamp(Some("2013-09-22 04:46:09.8246825417"))
            .expect("timestamp should parse");

        assert_eq!(parsed.to_rfc3339(), "2013-09-22T04:46:09.824682+00:00");
    }

    #[test]
    fn parse_grl_timestamp_maps_null_to_unix_epoch() {
        let parsed = parse_grl_timestamp(Some("null")).expect("null should map to epoch");

        assert_eq!(parsed, unix_epoch());
    }

    #[test]
    fn parse_grl_timestamp_maps_json_null_to_unix_epoch() {
        let parsed = parse_grl_timestamp(None).expect("missing timestamp should map to epoch");

        assert_eq!(parsed, unix_epoch());
    }
}