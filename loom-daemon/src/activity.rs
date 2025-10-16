use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Activity database for tracking agent inputs and results
pub struct ActivityDb {
    conn: Connection,
}

/// Type of input sent to terminal
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    Manual,     // User-initiated command
    Autonomous, // Agent autonomous action
    System,     // System-initiated (e.g., setup commands)
}

impl InputType {
    fn as_str(&self) -> &str {
        match self {
            Self::Manual => "manual",
            Self::Autonomous => "autonomous",
            Self::System => "system",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(Self::Manual),
            "autonomous" => Some(Self::Autonomous),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// Context information for an agent input
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InputContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_number: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<i32>,
}

/// Agent input record
#[derive(Debug, Clone)]
pub struct AgentInput {
    pub id: Option<i64>,
    pub terminal_id: String,
    pub timestamp: DateTime<Utc>,
    pub input_type: InputType,
    pub content: String,
    pub agent_role: Option<String>,
    pub context: InputContext,
}

impl ActivityDb {
    /// Create or open activity database at the given path
    pub fn new(db_path: PathBuf) -> Result<Self> {
        let conn = Connection::open(db_path)?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// Initialize database schema
    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS agent_inputs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                terminal_id TEXT NOT NULL,
                timestamp DATETIME NOT NULL,
                input_type TEXT NOT NULL,
                content TEXT NOT NULL,
                agent_role TEXT,
                context TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_inputs_terminal_id ON agent_inputs(terminal_id);
            CREATE INDEX IF NOT EXISTS idx_inputs_timestamp ON agent_inputs(timestamp);
            CREATE INDEX IF NOT EXISTS idx_inputs_type ON agent_inputs(input_type);

            CREATE TABLE IF NOT EXISTS agent_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                input_id INTEGER NOT NULL,
                result_type TEXT NOT NULL,
                timestamp DATETIME NOT NULL,
                status TEXT,
                data TEXT,
                FOREIGN KEY(input_id) REFERENCES agent_inputs(id)
            );

            CREATE INDEX IF NOT EXISTS idx_results_input_id ON agent_results(input_id);
            CREATE INDEX IF NOT EXISTS idx_results_timestamp ON agent_results(timestamp);

            CREATE TABLE IF NOT EXISTS agent_tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                description TEXT,
                started_at DATETIME,
                completed_at DATETIME,
                github_issue INTEGER,
                github_pr INTEGER
            );

            CREATE TABLE IF NOT EXISTS task_inputs (
                task_id INTEGER NOT NULL,
                input_id INTEGER NOT NULL,
                FOREIGN KEY(task_id) REFERENCES agent_tasks(id),
                FOREIGN KEY(input_id) REFERENCES agent_inputs(id),
                PRIMARY KEY(task_id, input_id)
            );
            "#,
        )?;

        Ok(())
    }

    /// Record a new agent input
    pub fn record_input(&self, input: &AgentInput) -> Result<i64> {
        let context_json = serde_json::to_string(&input.context)?;

        self.conn.execute(
            r#"
            INSERT INTO agent_inputs (terminal_id, timestamp, input_type, content, agent_role, context)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                &input.terminal_id,
                input.timestamp.to_rfc3339(),
                input.input_type.as_str(),
                &input.content,
                &input.agent_role,
                &context_json,
            ],
        )?;

        Ok(self.conn.last_insert_rowid())
    }

    /// Get recent inputs for a terminal
    pub fn get_recent_inputs(&self, terminal_id: &str, limit: usize) -> Result<Vec<AgentInput>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, terminal_id, timestamp, input_type, content, agent_role, context
            FROM agent_inputs
            WHERE terminal_id = ?1
            ORDER BY timestamp DESC
            LIMIT ?2
            "#,
        )?;

        let inputs = stmt.query_map(params![terminal_id, limit], |row| {
            let id: i64 = row.get(0)?;
            let terminal_id: String = row.get(1)?;
            let timestamp_str: String = row.get(2)?;
            let input_type_str: String = row.get(3)?;
            let content: String = row.get(4)?;
            let agent_role: Option<String> = row.get(5)?;
            let context_json: String = row.get(6)?;

            let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .with_timezone(&Utc);

            let input_type = InputType::from_str(&input_type_str)
                .ok_or_else(|| {
                    rusqlite::Error::ToSqlConversionFailure(
                        format!("Invalid input_type: {input_type_str}").into(),
                    )
                })?;

            let context: InputContext = serde_json::from_str(&context_json)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

            Ok(AgentInput {
                id: Some(id),
                terminal_id,
                timestamp,
                input_type,
                content,
                agent_role,
                context,
            })
        })?;

        inputs.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Get total count of inputs
    pub fn get_input_count(&self) -> Result<i64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM agent_inputs", [], |row| row.get(0))?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_create_database() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = ActivityDb::new(temp_file.path().to_path_buf()).unwrap();

        // Verify schema created successfully
        let count = db.get_input_count().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_record_and_retrieve_input() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = ActivityDb::new(temp_file.path().to_path_buf()).unwrap();

        let input = AgentInput {
            id: None,
            terminal_id: "terminal-1".to_string(),
            timestamp: Utc::now(),
            input_type: InputType::Manual,
            content: "ls -la".to_string(),
            agent_role: Some("worker".to_string()),
            context: InputContext {
                workspace: Some("/path/to/workspace".to_string()),
                branch: Some("main".to_string()),
                ..Default::default()
            },
        };

        let id = db.record_input(&input).unwrap();
        assert!(id > 0);

        let inputs = db.get_recent_inputs("terminal-1", 10).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].content, "ls -la");
        assert_eq!(inputs[0].agent_role, Some("worker".to_string()));
    }

    #[test]
    fn test_multiple_inputs() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = ActivityDb::new(temp_file.path().to_path_buf()).unwrap();

        for i in 0..5 {
            let input = AgentInput {
                id: None,
                terminal_id: "terminal-1".to_string(),
                timestamp: Utc::now(),
                input_type: InputType::Autonomous,
                content: format!("command {i}"),
                agent_role: Some("worker".to_string()),
                context: InputContext::default(),
            };
            db.record_input(&input).unwrap();
        }

        let inputs = db.get_recent_inputs("terminal-1", 3).unwrap();
        assert_eq!(inputs.len(), 3);

        let count = db.get_input_count().unwrap();
        assert_eq!(count, 5);
    }
}
