# Importer Usage

This project provides a CLI importer in [src/bin/import_events.rs](src/bin/import_events.rs).

Use it with:

```bash
cargo run --bin import_events -- \
  --db-user USER \
  --db-password PASSWORD \
  --db-host HOST \
  --db-name DATABASE \
  [--db-port 5432] \
  <subcommand> [subcommand options]
```

## Required Database Arguments

Every importer command requires these options:

- `--db-user`
- `--db-password`
- `--db-host`
- `--db-name`
- `--db-port` optional, default is `5432`

Example:

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  grl --input data/grl.json
```

## Import Order

Recommended order:

1. `grl`
2. `gcd`
3. `pfraw`
4. `step1`
5. `filter-rates`
6. `charge-distributions`
7. `charge-comparisons`

Notes:

- The importer can create placeholder `runs` rows automatically when needed, so `gcd`, file metadata, filter rates, and charge data can be loaded before GRL if necessary.
- `charge-comparisons` only updates an existing `charge_distributions` row. Import `charge-distributions` first.

## Stage Values

Commands that need a stage accept:

- `raw`
- `step1`
- `step2`

## Commands

### `pfraw`

Imports newline-delimited JSON metadata into `run_files`.

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  pfraw --input /path/to/files.ndjson
```

Expected input shape per line:

```json
{
  "uuid": "...",
  "logical_name": "/path/to/file",
  "checksum": { "sha512": "..." },
  "processing_level": "PFRaw",
  "run": {
    "run_number": 132304,
    "part_number": 33
  }
}
```

### `step1`

Imports Step 1 JSON metadata into `run_files`.

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  step1 --input /path/to/step1.json
```

Expected input shape:

```json
{
  "/path/to/archive": [
    {
      "logical_name": "/path/to/Run00132304_Subrun00000000_00000033.i3.zst",
      "checksum": { "sha512": "..." }
    }
  ]
}
```

The importer extracts run number and part number from the filename.

### `gcd`

Imports GCD files into `gcd_files`.

You can point it at a directory:

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  gcd --gcd-dir /path/to/gcds --stage step1
```

Or provide a newline-delimited list file:

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  gcd --list-file /path/to/gcd_files.txt --stage step1
```

List file format:

```text
/path/to/Run132304_GCD.i3.zst
/path/to/Run132305_GCD.i3.zst
```

The importer:

- filters for filenames containing `GCD` and ending in `.i3.zst`
- extracts the run number from `Run123456`
- computes SHA512
- skips a file if the same SHA512 already exists in `gcd_files`

### `grl`

Imports GRL JSON into `runs`.

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  grl --input data/grl.json
```

Current behavior:

- only records with `good_i3: true` are imported
- `good_tstart` becomes `run_start_date`
- `good_tstop` becomes `run_end_date`
- `url` is set to the IceCube live run URL

### `filter-rates`

Imports filter-rate JSON files into `filter_rates`.

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  filter-rates --input /path/to/filter_rate_dir --stage step1
```

The input can be either:

- a directory containing `.filter_rates.txt` files
- a single `.filter_rates.txt` file

Example filename:

```text
Run132304.filter_rates.txt
```

The run number is extracted from the filename.

Expected JSON shape:

```json
{
  "files_cover": 28816.59,
  "header_count": 42323575,
  "frame_count": 81930846,
  "overall_frame_rate": 2843.18,
  "filter_rates": {
    "Keep_SuperDST_23": 1467.34,
    "MuonFilter_23": 33.16
  }
}
```

Only the nested `filter_rates` object is stored in the `filter_rates` table.

### `charge-distributions`

Imports NPZ files into `charge_distributions`.

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  charge-distributions --input /path/to/charge_npz_dir --stage step1
```

The input can be either:

- a directory containing `.npz` files
- a single `.npz` file

Example filename:

```text
Run132304.fadc_atwd_charge.npz
```

The importer currently expects these NPZ entries:

- `bounds`
- `start`
- `atwd`
- `atwd_mean`
- `atwd_sigma`
- `fadc`
- `fadc_mean`
- `fadc_sigma`
- `bins`
- `allow_pickle`

Storage layout:

- `atwd_histograms` stores shared metadata plus the ATWD histogram, mean, and sigma arrays
- `fadc_histograms` stores shared metadata plus the FADC histogram, mean, and sigma arrays
- arrays are serialized as JSON objects with `dtype`, `shape`, and flat `values`

### `charge-comparisons`

Imports LLH comparison JSON files into `charge_distributions.llh_comparison`.

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  charge-comparisons --input /path/to/comparison_dir --stage step1
```

The input can be either:

- a directory containing `.json` files
- a single comparison `.json` file

Example shape:

```json
{
  "logL_corr": -680.22,
  "logL_uncorr": -4657.06,
  "delta_logL": 3976.84,
  "pearson_r": 0.7364,
  "std_atwd": 0.0237,
  "std_fadc": 0.0229,
  "data_file": "/data/.../Run132304.fadc_atwd_charge.npz",
  "corrected_template_file": "/data/.../Run136692.fadc_atwd_charge.npz",
  "uncorrected_template_file": "/data/.../Run140950.fadc_atwd_charge.npz"
}
```

Run number detection:

- first tries `data_file`
- falls back to the JSON filename

Important:

- this command does not create a new `charge_distributions` row
- import `charge-distributions` first for the same run and stage

### `inspect-charge`

Reads a `charge_distributions` row back out of Postgres.

Summary view:

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  inspect-charge --run-number 132304 --stage step1
```

Full JSON view:

```bash
cargo run --bin import_events -- \
  --db-user postgres \
  --db-password postgres \
  --db-host localhost \
  --db-name calendar \
  inspect-charge --run-number 132304 --stage step1 --full-json
```

The summary view prints a reduced shape-oriented view instead of the full histogram payload, which is useful for checking that the row exists and has the expected structure.

## Common Failures

### Comparison import is skipped

If you see:

```text
Skipping comparison for run 132304: charge histograms must be imported first
```

run `charge-distributions` first for the same run and stage.

### No rows found in `inspect-charge`

This means there is no `charge_distributions` row for that `run_number` and `stage`.

### GCD file was skipped

Common reasons:

- filename does not contain `GCD`
- filename does not end in `.i3.zst`
- run number could not be extracted from `Run123456`
- the file SHA512 is already present in `gcd_files`

## Quick Examples

Load one run end to end:

```bash
cargo run --bin import_events -- --db-user postgres --db-password postgres --db-host localhost --db-name calendar grl --input data/grl.json
cargo run --bin import_events -- --db-user postgres --db-password postgres --db-host localhost --db-name calendar gcd --list-file /tmp/gcd_files.txt --stage step1
cargo run --bin import_events -- --db-user postgres --db-password postgres --db-host localhost --db-name calendar filter-rates --input /data/filter_rates/Run132304.filter_rates.txt --stage step1
cargo run --bin import_events -- --db-user postgres --db-password postgres --db-host localhost --db-name calendar charge-distributions --input /data/charge/Run132304.fadc_atwd_charge.npz --stage step1
cargo run --bin import_events -- --db-user postgres --db-password postgres --db-host localhost --db-name calendar charge-comparisons --input /data/charge/Run132304.comparison.json --stage step1
cargo run --bin import_events -- --db-user postgres --db-password postgres --db-host localhost --db-name calendar inspect-charge --run-number 132304 --stage step1
```