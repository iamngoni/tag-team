CREATE TABLE nodes (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    os TEXT,
    arch TEXT,
    os_version TEXT,
    state TEXT NOT NULL,
    trust TEXT NOT NULL,
    presence JSON,
    cert_sha256 BLOB NOT NULL,
    capacity JSON,
    toolchains JSON,
    repos JSON,
    policy JSON NOT NULL,
    last_seen_at INTEGER,
    revoked_at INTEGER
);

CREATE TABLE invites (
    code_sha256 BLOB PRIMARY KEY,
    name TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    node_id TEXT REFERENCES nodes(id)
);

CREATE TABLE dispatches (
    id TEXT PRIMARY KEY,
    main_session TEXT NOT NULL,
    claude_session TEXT,
    repo TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE subtasks (
    id TEXT PRIMARY KEY,
    dispatch_id TEXT NOT NULL REFERENCES dispatches(id),
    title TEXT NOT NULL,
    spec JSON NOT NULL,
    state JSON NOT NULL,
    node_id TEXT REFERENCES nodes(id),
    attempt INTEGER NOT NULL DEFAULT 0,
    base TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    branch TEXT NOT NULL,
    runner_session TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    usage JSON NOT NULL
);

CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    subtask_id TEXT NOT NULL REFERENCES subtasks(id),
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    body JSON NOT NULL
);

CREATE TABLE questions (
    id TEXT PRIMARY KEY,
    subtask_id TEXT NOT NULL REFERENCES subtasks(id),
    text TEXT NOT NULL,
    options JSON NOT NULL,
    context TEXT,
    deadline_at INTEGER NOT NULL,
    answer TEXT,
    answered_by JSON
);

CREATE TABLE results (
    subtask_id TEXT PRIMARY KEY REFERENCES subtasks(id),
    outcome TEXT NOT NULL,
    summary TEXT NOT NULL,
    bundle_sha256 BLOB,
    bundle_size INTEGER,
    branch TEXT,
    diffstat JSON NOT NULL,
    assumptions JSON NOT NULL,
    checks JSON NOT NULL
);

CREATE TABLE commands (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL REFERENCES nodes(id),
    subtask_id TEXT NOT NULL REFERENCES subtasks(id),
    op JSON NOT NULL,
    state TEXT NOT NULL,
    result JSON,
    sent_at INTEGER,
    done_at INTEGER
);

CREATE INDEX events_by_subtask ON events(subtask_id, seq);
CREATE INDEX open_questions ON questions(deadline_at) WHERE answer IS NULL;
CREATE INDEX pending_commands_by_node ON commands(node_id, state, sent_at);
