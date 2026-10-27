ALTER TABLE questions ADD COLUMN call_id TEXT NOT NULL DEFAULT '';
ALTER TABLE questions ADD COLUMN answered_at INTEGER;
ALTER TABLE questions ADD COLUMN closed_at INTEGER;

CREATE INDEX open_questions_by_subtask
    ON questions(subtask_id, deadline_at)
    WHERE answer IS NULL AND closed_at IS NULL;
