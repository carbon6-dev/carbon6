-- Follow-through (kits, next steps, trackers), self-configuration and
-- playbooks were removed; their tables and settings go too. (0024 and 0026
-- stay: databases have applied them.)
DROP TABLE IF EXISTS tracker_observations;
DROP TABLE IF EXISTS trackers;
DROP TABLE IF EXISTS suggestions;
DELETE FROM settings WHERE key IN ('kits', 'playbooks', 'playbook_runs', 'followthrough');
