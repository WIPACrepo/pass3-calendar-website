-- Create workflow state enum
CREATE TYPE workflow_state AS ENUM (
    'Not Yet Started',
    'Transfer from Tape',
    'Process Step 1',
    'Finish Step 1',
    'Transfer WIPAC',
    'Process Step 2',
    'Finish Step 2',
    'Complete',
    'Step 1 Error',
    'Step 2 Error'
);

-- Create runs table
CREATE TABLE IF NOT EXISTS runs (
    run_number INT PRIMARY KEY,
    file_number INT NOT NULL,
    run_start_date TIMESTAMP NOT NULL,
    state workflow_state NOT NULL DEFAULT 'Not Yet Started',
    url TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Create processing_steps table for tracking Step 1 and Step 2
CREATE TABLE IF NOT EXISTS processing_steps (
    id UUID PRIMARY KEY,
    run_number INT NOT NULL,
    step_number INT NOT NULL,
    started_date TIMESTAMP,
    end_date TIMESTAMP,
    site TEXT,
    checksum TEXT,
    location TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (run_number) REFERENCES runs(run_number) ON DELETE CASCADE,
    UNIQUE(run_number, step_number)
);

-- Create indexes for faster queries
CREATE INDEX IF NOT EXISTS idx_runs_state ON runs(state);
CREATE INDEX IF NOT EXISTS idx_runs_start_date ON runs(run_start_date);
CREATE INDEX IF NOT EXISTS idx_steps_run_number ON processing_steps(run_number);
CREATE INDEX IF NOT EXISTS idx_steps_step_number ON processing_steps(step_number);
CREATE INDEX IF NOT EXISTS idx_steps_site ON processing_steps(site);
