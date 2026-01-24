import { beforeEach, describe, expect, it, vi } from "vitest";
import { onRequest } from "../[[route]]";

// Mock D1 database
function createMockDb() {
  const mockStatement = {
    bind: vi.fn().mockReturnThis(),
    all: vi.fn(),
    first: vi.fn(),
    run: vi.fn(),
  };

  return {
    prepare: vi.fn().mockReturnValue(mockStatement),
    _statement: mockStatement,
  };
}

function createContext(
  method: string,
  path: string,
  options: {
    body?: unknown;
    db?: ReturnType<typeof createMockDb>;
    appName?: string;
  } = {},
) {
  const url = new URL(path, "http://localhost");
  const route = path.replace("/api/", "").split("/").filter(Boolean);

  const request = new Request(url.toString(), {
    method,
    headers: { "Content-Type": "application/json" },
    body: options.body ? JSON.stringify(options.body) : undefined,
  });

  return {
    request,
    env: {
      DB: options.db ?? createMockDb(),
      APP_NAME: options.appName ?? "test-app",
    },
    params: { route: route.length ? route : undefined },
    waitUntil: vi.fn(),
    passThroughOnException: vi.fn(),
    next: vi.fn(),
    data: {},
  } as unknown as Parameters<typeof onRequest>[0];
}

describe("API Routes", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  describe("GET /api/health", () => {
    it("returns healthy status when database is available", async () => {
      const mockDb = createMockDb();
      mockDb._statement.first.mockResolvedValue({ 1: 1 });

      const context = createContext("GET", "/api/health", { db: mockDb });
      const response = await onRequest(context);

      expect(response.status).toBe(200);

      const data = await response.json();
      expect(data.status).toBe("healthy");
      expect(data.app).toBe("test-app");
      expect(data.timestamp).toBeDefined();
    });

    it("returns unhealthy status when database fails", async () => {
      const mockDb = createMockDb();
      mockDb._statement.first.mockRejectedValue(new Error("DB connection failed"));

      const context = createContext("GET", "/api/health", { db: mockDb });
      const response = await onRequest(context);

      expect(response.status).toBe(503);

      const data = await response.json();
      expect(data.status).toBe("unhealthy");
      expect(data.error).toBe("Database unavailable");
    });
  });

  describe("GET /api/users", () => {
    it("returns list of users", async () => {
      const mockUsers = [
        { id: "1", email: "user1@example.com", name: "User 1", created_at: "2024-01-01" },
        { id: "2", email: "user2@example.com", name: "User 2", created_at: "2024-01-02" },
      ];

      const mockDb = createMockDb();
      mockDb._statement.all.mockResolvedValue({ results: mockUsers });

      const context = createContext("GET", "/api/users", { db: mockDb });
      const response = await onRequest(context);

      expect(response.status).toBe(200);

      const data = await response.json();
      expect(data.users).toEqual(mockUsers);
    });

    it("returns empty array when no users exist", async () => {
      const mockDb = createMockDb();
      mockDb._statement.all.mockResolvedValue({ results: [] });

      const context = createContext("GET", "/api/users", { db: mockDb });
      const response = await onRequest(context);

      expect(response.status).toBe(200);

      const data = await response.json();
      expect(data.users).toEqual([]);
    });
  });

  describe("GET /api/users/:id", () => {
    it("returns user when found", async () => {
      const mockUser = {
        id: "123",
        email: "test@example.com",
        name: "Test User",
        created_at: "2024-01-01",
      };

      const mockDb = createMockDb();
      mockDb._statement.first.mockResolvedValue(mockUser);

      const context = createContext("GET", "/api/users/123", { db: mockDb });
      const response = await onRequest(context);

      expect(response.status).toBe(200);

      const data = await response.json();
      expect(data.user).toEqual(mockUser);
    });

    it("returns 404 when user not found", async () => {
      const mockDb = createMockDb();
      mockDb._statement.first.mockResolvedValue(null);

      const context = createContext("GET", "/api/users/nonexistent", { db: mockDb });
      const response = await onRequest(context);

      expect(response.status).toBe(404);

      const data = await response.json();
      expect(data.error).toBe("User not found");
    });
  });

  describe("POST /api/users", () => {
    it("creates user with valid data", async () => {
      const mockDb = createMockDb();
      mockDb._statement.run.mockResolvedValue({ success: true });

      const context = createContext("POST", "/api/users", {
        db: mockDb,
        body: {
          email: "new@example.com",
          name: "New User",
          password: "password123",
        },
      });

      const response = await onRequest(context);

      expect(response.status).toBe(201);

      const data = await response.json();
      expect(data.user.email).toBe("new@example.com");
      expect(data.user.name).toBe("New User");
      expect(data.user.id).toBeDefined();
    });

    it("returns 400 for missing email", async () => {
      const mockDb = createMockDb();

      const context = createContext("POST", "/api/users", {
        db: mockDb,
        body: {
          name: "New User",
          password: "password123",
        },
      });

      const response = await onRequest(context);

      expect(response.status).toBe(400);

      const data = await response.json();
      expect(data.error).toBe("Email, name, and password are required");
    });

    it("returns 400 for missing password", async () => {
      const mockDb = createMockDb();

      const context = createContext("POST", "/api/users", {
        db: mockDb,
        body: {
          email: "test@example.com",
          name: "Test User",
        },
      });

      const response = await onRequest(context);

      expect(response.status).toBe(400);

      const data = await response.json();
      expect(data.error).toBe("Email, name, and password are required");
    });

    it("returns 409 for duplicate email", async () => {
      const mockDb = createMockDb();
      mockDb._statement.run.mockRejectedValue(new Error("UNIQUE constraint failed: users.email"));

      const context = createContext("POST", "/api/users", {
        db: mockDb,
        body: {
          email: "existing@example.com",
          name: "User",
          password: "password",
        },
      });

      const response = await onRequest(context);

      expect(response.status).toBe(409);

      const data = await response.json();
      expect(data.error).toBe("Email already exists");
    });
  });

  describe("Unknown routes", () => {
    it("returns 404 for unknown path", async () => {
      const context = createContext("GET", "/api/unknown");
      const response = await onRequest(context);

      expect(response.status).toBe(404);

      const data = await response.json();
      expect(data.error).toBe("Not found");
    });

    it("returns 404 for wrong method on valid path", async () => {
      const context = createContext("DELETE", "/api/users");
      const response = await onRequest(context);

      expect(response.status).toBe(404);
    });
  });
});
