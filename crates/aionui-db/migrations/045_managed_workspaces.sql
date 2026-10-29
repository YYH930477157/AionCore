-- Draft roots remain in project_explorer; the saved snapshot is independent
-- of filesystem contents and can restore a removed root with its stable ID.
CREATE TABLE IF NOT EXISTS project_workspaces (
    project_id TEXT PRIMARY KEY,
    owner_user_id TEXT NOT NULL,
    config_path TEXT,
    dirty INTEGER NOT NULL DEFAULT 1,
    default_pe_id TEXT,
    saved_json TEXT
);
CREATE INDEX IF NOT EXISTS idx_project_workspaces_owner ON project_workspaces(owner_user_id);
