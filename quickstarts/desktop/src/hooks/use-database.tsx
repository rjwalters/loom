import { createContext, useContext, useEffect, useState, useCallback } from "react";
import Database from "@tauri-apps/plugin-sql";

interface Note {
  id: number;
  title: string;
  content: string;
  created_at: string;
  updated_at: string;
}

interface DatabaseContextValue {
  isLoading: boolean;
  error: string | null;
  notes: Note[];
  createNote: (title: string, content: string) => Promise<Note>;
  updateNote: (id: number, title: string, content: string) => Promise<void>;
  deleteNote: (id: number) => Promise<void>;
  refreshNotes: () => Promise<void>;
}

const DatabaseContext = createContext<DatabaseContextValue | undefined>(undefined);

export function DatabaseProvider({ children }: { children: React.ReactNode }) {
  const [db, setDb] = useState<Database | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notes, setNotes] = useState<Note[]>([]);

  // Initialize database
  useEffect(() => {
    const initDb = async () => {
      try {
        const database = await Database.load("sqlite:data.db");

        // Create notes table if it doesn't exist
        await database.execute(`
          CREATE TABLE IF NOT EXISTS notes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            title TEXT NOT NULL,
            content TEXT NOT NULL DEFAULT '',
            created_at TEXT DEFAULT (datetime('now')),
            updated_at TEXT DEFAULT (datetime('now'))
          )
        `);

        setDb(database);

        // Load initial notes
        const result = await database.select<Note[]>(
          "SELECT * FROM notes ORDER BY updated_at DESC"
        );
        setNotes(result);
      } catch (e) {
        setError((e as Error).message);
      } finally {
        setIsLoading(false);
      }
    };

    initDb();
  }, []);

  const refreshNotes = useCallback(async () => {
    if (!db) return;
    try {
      const result = await db.select<Note[]>(
        "SELECT * FROM notes ORDER BY updated_at DESC"
      );
      setNotes(result);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [db]);

  const createNote = useCallback(
    async (title: string, content: string): Promise<Note> => {
      if (!db) throw new Error("Database not initialized");

      const result = await db.execute(
        "INSERT INTO notes (title, content) VALUES (?, ?)",
        [title, content]
      );

      const newNote = await db.select<Note[]>(
        "SELECT * FROM notes WHERE id = ?",
        [result.lastInsertId]
      );

      await refreshNotes();
      return newNote[0];
    },
    [db, refreshNotes]
  );

  const updateNote = useCallback(
    async (id: number, title: string, content: string) => {
      if (!db) throw new Error("Database not initialized");

      await db.execute(
        "UPDATE notes SET title = ?, content = ?, updated_at = datetime('now') WHERE id = ?",
        [title, content, id]
      );

      await refreshNotes();
    },
    [db, refreshNotes]
  );

  const deleteNote = useCallback(
    async (id: number) => {
      if (!db) throw new Error("Database not initialized");

      await db.execute("DELETE FROM notes WHERE id = ?", [id]);
      await refreshNotes();
    },
    [db, refreshNotes]
  );

  return (
    <DatabaseContext.Provider
      value={{
        isLoading,
        error,
        notes,
        createNote,
        updateNote,
        deleteNote,
        refreshNotes,
      }}
    >
      {children}
    </DatabaseContext.Provider>
  );
}

export function useDatabase() {
  const context = useContext(DatabaseContext);
  if (context === undefined) {
    throw new Error("useDatabase must be used within a DatabaseProvider");
  }
  return context;
}
