-- Managed multi-root workspaces.
--
-- A project is either the legacy single-root kind (one `workspace` entry plus removable
-- attached folders) or a managed workspace: an ordered set of roots with a mutable default
-- root, whose configuration can be saved to and reopened from a `.woody-workspace` file.
--
-- The state lives on `projects` because there is exactly one workspace per project and it must
-- travel with the row that already carries ownership; roots stay ordinary `project_explorer`
-- rows, so ordering, containment and the source-control seam keep working unchanged.
ALTER TABLE projects ADD COLUMN workspace_managed INTEGER NOT NULL DEFAULT 0;

-- Where this workspace's configuration file lives; NULL until it has been saved somewhere.
ALTER TABLE projects ADD COLUMN workspace_config_path TEXT;

-- The root new conversations start in. Replaces the immutable `role = 'workspace'` entry for
-- managed workspaces: it is a soft reference validated by the service, so removing a root never
-- needs to rewrite the column inside the same statement.
ALTER TABLE projects ADD COLUMN workspace_default_pe_id TEXT;

-- Set when the in-database roots differ from what `workspace_config_path` holds; cleared by save
-- and by discard. Always 0 while no file exists, so an unsaved workspace is never "dirty".
ALTER TABLE projects ADD COLUMN workspace_dirty INTEGER NOT NULL DEFAULT 0;
