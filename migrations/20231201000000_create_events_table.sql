-- Create workflow state enum
CREATE TYPE workflow_state AS ENUM (
    'Not Yet Started',
    'Step 1 GCD Generated',
    'Transfer from Tape',
    'Process Step 1',
    'Finish Step 1',
    'Transfer WIPAC',
    'Step 2 GCD Generated',
    'Process Step 2',
    'Finish Step 2',
    'Complete',
    'Step 1 Error',
    'Step 2 Error'
);

-- Create stage enum
CREATE TYPE stage as ENUM (
    'Raw Data',
    'Step 1',
    'Step 2'
);

-- Create runs table
CREATE TABLE IF NOT EXISTS runs (
    run_number INT PRIMARY KEY,
    run_start_date TIMESTAMP NOT NULL,
    run_end_date TIMESTAMP NOT NULL,
    state workflow_state NOT NULL DEFAULT 'Not Yet Started',
    url TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Create processing_steps table for tracking Step 1 and Step 2
CREATE TABLE IF NOT EXISTS processing_steps (
    id UUID PRIMARY KEY,
    run_number INT NOT NULL,
    stage stage NOT NULL,
    started_date TIMESTAMP,
    end_date TIMESTAMP,
    site TEXT,
    location TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (run_number) REFERENCES runs(run_number) ON DELETE CASCADE,
    UNIQUE(run_number, stage)
);

-- Create gcd_files table for tracking GCD file locations
CREATE TABLE IF NOT EXISTS gcd_files (
    id UUID PRIMARY KEY,
    run_number INT NOT NULL,
    stage stage NOT NULL,
    location TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (run_number) REFERENCES runs(run_number) ON DELETE CASCADE,
    UNIQUE(run_number, stage)
);

-- Create run_files table with stage marker
CREATE TABLE IF NOT EXISTS run_files (
    id UUID PRIMARY KEY,
    run_number INT NOT NULL,
    part_number INT NOT NULL,
    stage stage NOT NULL,
    file_path TEXT NOT NULL,
    sha512 CHAR(128) NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (run_number) REFERENCES runs(run_number) ON DELETE CASCADE
);

-- Create run_files table with broken marker
CREATE TABLE IF NOT EXISTS broken_files (
    id UUID PRIMARY KEY,
    run_number INT NOT NULL,
    part_number INT NOT NULL,
    stage stage NOT NULL,
    file_path TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (run_number) REFERENCES runs(run_number) ON DELETE CASCADE
);

-- Create run_notes table for storing notes per run
CREATE TABLE IF NOT EXISTS run_notes (
    id UUID PRIMARY KEY,
    run_number INT NOT NULL,
    note TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (run_number) REFERENCES runs(run_number) ON DELETE CASCADE,
    UNIQUE(run_number)
);

-- Create indexes for faster queries
CREATE INDEX IF NOT EXISTS idx_runs_state ON runs(state);
CREATE INDEX IF NOT EXISTS idx_runs_start_date ON runs(run_start_date);
CREATE INDEX IF NOT EXISTS idx_steps_run_number ON processing_steps(run_number);
CREATE INDEX IF NOT EXISTS idx_steps_stage ON processing_steps(stage);
CREATE INDEX IF NOT EXISTS idx_steps_site ON processing_steps(site);
CREATE INDEX IF NOT EXISTS idx_run_files_run_stage ON run_files(run_number, stage);
CREATE INDEX IF NOT EXISTS idx_run_files_sha ON run_files(sha512);
CREATE INDEX IF NOT EXISTS idx_run_notes_run_number ON run_notes(run_number);