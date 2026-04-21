use clap::{Args, Parser, Subcommand, ValueEnum};
use pass3_calendar_website::{
    importers::{
        import_charge_comparison_files, import_charge_distribution_files, import_filter_rate_files,
        import_gcd_files, import_grl_file, import_pfraw_file, import_step1_file,
        inspect_charge_distribution,
        is_charge_comparison_file, is_charge_distribution_file, is_filter_rate_file,
        is_gcd_file, list_matching_files, read_path_list,
    },
    Stage,
};
use sqlx::postgres::PgPoolOptions;
use std::path::Path;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    db: DatabaseArgs,

    #[command(subcommand)]
    command: ImportCommand,
}

#[derive(Args, Debug)]
struct DatabaseArgs {
    /// Database user
    #[arg(long)]
    db_user: String,

    /// Database password
    #[arg(long)]
    db_password: String,

    /// Database host
    #[arg(long)]
    db_host: String,

    /// Database port
    #[arg(long, default_value = "5432")]
    db_port: String,

    /// Database name
    #[arg(long)]
    db_name: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StageArg {
    Raw,
    Step1,
    Step2,
}

impl From<StageArg> for Stage {
    fn from(value: StageArg) -> Self {
        match value {
            StageArg::Raw => Stage::RawData,
            StageArg::Step1 => Stage::Step1,
            StageArg::Step2 => Stage::Step2,
        }
    }
}

#[derive(Subcommand, Debug)]
enum ImportCommand {
    /// Import PFRaw NDJSON file metadata into run_files.
    Pfraw {
        #[arg(long)]
        input: String,
    },
    /// Import Step 1 JSON file metadata into run_files.
    Step1 {
        #[arg(long)]
        input: String,
    },

    /// Import GCD files from either a directory or a newline-delimited list file.
    Gcd {
        #[arg(long, conflicts_with = "list_file")]
        gcd_dir: Option<String>,
        #[arg(long, conflicts_with = "gcd_dir")]
        list_file: Option<String>,

        #[arg(long, value_enum)]
        stage: StageArg,
    },

    /// Import run metadata from a GRL JSON file.
    Grl {
        #[arg(long)]
        input: String,
    },

    /// Import filter-rate JSON files into filter_rates.
    FilterRates {
        #[arg(long)]
        input: String,

        #[arg(long, value_enum)]
        stage: StageArg,
    },

    /// Import charge-distribution NPZ files into charge_distributions.
    ChargeDistributions {
        #[arg(long)]
        input: String,
        #[arg(long, value_enum)]
        stage: StageArg,
    },
    /// Import LLH comparison JSON files into charge_distributions.
    ChargeComparisons {
        #[arg(long)]
        input: String,

        #[arg(long, value_enum)]
        stage: StageArg,
    },

    /// Inspect a charge-distribution row stored in Postgres.
    InspectCharge {
        #[arg(long)]
        run_number: i32,

        #[arg(long, value_enum)]
        stage: StageArg,

        #[arg(long)]
        full_json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Cli::parse();

    let database_url = format!(
        "postgres://{}:{}@{}:{}/{}",
        args.db.db_user, args.db.db_password, args.db.db_host, args.db.db_port, args.db.db_name
    );
    println!("Connecting to database at {}:{}...", args.db.db_host, args.db.db_port);

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await?;

    let report = match args.command {
        ImportCommand::Pfraw { input } => {
            println!("Importing PFRaw NDJSON file: {}", input);
            import_pfraw_file(&pool, Path::new(&input)).await?
        }
        ImportCommand::Step1 { input } => {
            println!("Importing Step 1 JSON file: {}", input);
            import_step1_file(&pool, Path::new(&input)).await?
        }
        ImportCommand::Gcd {
            gcd_dir,
            list_file,
            stage,
        } => {
            let paths = if let Some(gcd_dir) = gcd_dir {
                println!("Importing GCD files from directory: {}", gcd_dir);
                list_matching_files(Path::new(&gcd_dir), is_gcd_file)?
            } else if let Some(list_file) = list_file {
                println!("Importing GCD files from list: {}", list_file);
                read_path_list(Path::new(&list_file))?
                    .into_iter()
                    .filter(|path| is_gcd_file(path))
                    .collect()
            } else {
                return Err("either --gcd-dir or --list-file is required".into());
            };

            println!("Found {} GCD files to process", paths.len());
            import_gcd_files(&pool, stage.into(), &paths).await?
        }
        ImportCommand::Grl { input } => {
            println!("Importing GRL JSON file: {}", input);
            import_grl_file(&pool, Path::new(&input)).await?
        }
        ImportCommand::FilterRates { input, stage } => {
            let paths = list_matching_files(Path::new(&input), is_filter_rate_file)?;
            println!("Found {} filter-rate files to process", paths.len());
            import_filter_rate_files(&pool, stage.into(), &paths).await?
        }
        ImportCommand::ChargeDistributions { input, stage } => {
            let paths = list_matching_files(Path::new(&input), is_charge_distribution_file)?;
            println!("Found {} charge-distribution files to process", paths.len());
            import_charge_distribution_files(&pool, stage.into(), &paths).await?
        }
        ImportCommand::ChargeComparisons { input, stage } => {
            let paths = list_matching_files(Path::new(&input), is_charge_comparison_file)?;
            println!("Found {} charge-comparison files to process", paths.len());
            import_charge_comparison_files(&pool, stage.into(), &paths).await?
        }
        ImportCommand::InspectCharge {
            run_number,
            stage,
            full_json,
        } => {
            let stage = Stage::from(stage);
            let row = inspect_charge_distribution(&pool, run_number, stage, full_json).await?;
            if let Some(row) = row {
                println!("{}", serde_json::to_string_pretty(&row)?);
            } else {
                println!(
                    "No charge_distributions row found for run {} at stage {:?}",
                    run_number, stage
                );
            }
            return Ok(());
        }
    };

    println!("\n=== Import Complete ===");
    println!("Imported: {}", report.imported);
    println!("Skipped:  {}", report.skipped);

    Ok(())
}
