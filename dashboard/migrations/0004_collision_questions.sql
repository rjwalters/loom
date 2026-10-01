-- Collision questions (#9906): the shared daily YES/NO question surface.
-- Question definitions live in code (src/questions.ts, versioned); D1 stores
-- the answer history so flips and stale answers are inspectable.

CREATE TABLE IF NOT EXISTS question_answers (
  question_id       TEXT NOT NULL,
  question_version  INTEGER NOT NULL,
  answer_date       TEXT NOT NULL,          -- UTC YYYY-MM-DD (idempotency key half)
  answer            TEXT NOT NULL,          -- 'YES' | 'NO'
  confidence_pct    INTEGER NOT NULL,
  summary           TEXT NOT NULL,
  change_summary    TEXT NOT NULL,
  evidence_status   TEXT NOT NULL,          -- sufficient | limited | insufficient
  missing_evidence  TEXT NOT NULL DEFAULT '[]',
  evidence_cutoff   TEXT NOT NULL,
  model_id          TEXT NOT NULL,
  run_kind          TEXT NOT NULL,          -- scheduled | manual
  recorded_at       TEXT NOT NULL,
  PRIMARY KEY (question_id, question_version, answer_date, run_kind)
);

CREATE INDEX IF NOT EXISTS idx_question_answers_recent
  ON question_answers (question_id, recorded_at DESC);
