// Cloudflare Pages Functions API handler
// This provides a simple API layer for the frontend

interface Env {
  DB: D1Database;
  APP_NAME: string;
}

interface User {
  id: string;
  email: string;
  name: string;
  created_at: string;
}

interface Project {
  id: string;
  user_id: string;
  name: string;
  description: string | null;
  status: "active" | "archived";
  created_at: string;
  updated_at: string;
}

interface Session {
  id: string;
  user_id: string;
  expires_at: string;
}

// Session configuration
const SESSION_COOKIE_NAME = "loom-session";
const SESSION_DURATION_MS = 7 * 24 * 60 * 60 * 1000; // 7 days

// Simple response helpers
function json(data: unknown, status = 200, headers: Record<string, string> = {}) {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json", ...headers },
  });
}

function error(message: string, status = 400) {
  return json({ error: message }, status);
}

// Cookie helpers
function setSessionCookie(sessionId: string, maxAge: number): string {
  return `${SESSION_COOKIE_NAME}=${sessionId}; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=${maxAge}`;
}

function clearSessionCookie(): string {
  return `${SESSION_COOKIE_NAME}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0`;
}

function getSessionIdFromCookie(request: Request): string | null {
  const cookieHeader = request.headers.get("Cookie") || "";
  const cookies = Object.fromEntries(
    cookieHeader.split(";").map((c) => {
      const [key, ...val] = c.trim().split("=");
      return [key, val.join("=")];
    }),
  );
  return cookies[SESSION_COOKIE_NAME] || null;
}

// Password hashing using PBKDF2 (edge-compatible, more secure than plain SHA-256)
async function hashPassword(
  password: string,
  salt?: string,
): Promise<{ hash: string; salt: string }> {
  const encoder = new TextEncoder();
  const passwordSalt =
    salt || btoa(String.fromCharCode(...crypto.getRandomValues(new Uint8Array(16))));

  const keyMaterial = await crypto.subtle.importKey(
    "raw",
    encoder.encode(password),
    "PBKDF2",
    false,
    ["deriveBits"],
  );

  const derivedBits = await crypto.subtle.deriveBits(
    {
      name: "PBKDF2",
      salt: encoder.encode(passwordSalt),
      iterations: 100000,
      hash: "SHA-256",
    },
    keyMaterial,
    256,
  );

  const hash = btoa(String.fromCharCode(...new Uint8Array(derivedBits)));
  return { hash: `${passwordSalt}:${hash}`, salt: passwordSalt };
}

async function verifyPassword(password: string, storedHash: string): Promise<boolean> {
  const [salt, expectedHash] = storedHash.split(":");
  if (!salt || !expectedHash) return false;

  const { hash } = await hashPassword(password, salt);
  const [, computedHash] = hash.split(":");

  // Constant-time comparison to prevent timing attacks
  if (computedHash.length !== expectedHash.length) return false;
  let result = 0;
  for (let i = 0; i < computedHash.length; i++) {
    result |= computedHash.charCodeAt(i) ^ expectedHash.charCodeAt(i);
  }
  return result === 0;
}

// Session management
async function createSession(env: Env, userId: string): Promise<string> {
  const sessionId = crypto.randomUUID();
  const expiresAt = new Date(Date.now() + SESSION_DURATION_MS).toISOString();

  await env.DB.prepare("INSERT INTO sessions (id, user_id, expires_at) VALUES (?, ?, ?)")
    .bind(sessionId, userId, expiresAt)
    .run();

  return sessionId;
}

async function getSessionUser(env: Env, sessionId: string): Promise<User | null> {
  const result = await env.DB.prepare(`
    SELECT u.id, u.email, u.name, u.created_at
    FROM sessions s
    JOIN users u ON s.user_id = u.id
    WHERE s.id = ? AND s.expires_at > datetime('now')
  `)
    .bind(sessionId)
    .first<User>();

  return result || null;
}

async function deleteSession(env: Env, sessionId: string): Promise<void> {
  await env.DB.prepare("DELETE FROM sessions WHERE id = ?").bind(sessionId).run();
}

async function refreshSession(env: Env, sessionId: string): Promise<void> {
  const expiresAt = new Date(Date.now() + SESSION_DURATION_MS).toISOString();
  await env.DB.prepare("UPDATE sessions SET expires_at = ? WHERE id = ?")
    .bind(expiresAt, sessionId)
    .run();
}

// Clean up expired sessions (call periodically)
async function cleanupExpiredSessions(env: Env): Promise<void> {
  await env.DB.prepare("DELETE FROM sessions WHERE expires_at < datetime('now')").run();
}

// Get authenticated user from session cookie
async function getAuthenticatedUser(env: Env, request: Request): Promise<User | null> {
  const sessionId = getSessionIdFromCookie(request);
  if (!sessionId) return null;
  return getSessionUser(env, sessionId);
}

// Auth handlers
async function handleLogin(env: Env, request: Request): Promise<Response> {
  const body = (await request.json()) as { email?: string; password?: string };
  const { email, password } = body;

  if (!email || !password) {
    return error("Email and password are required");
  }

  const user = await env.DB.prepare(
    "SELECT id, email, name, password_hash, created_at FROM users WHERE email = ?",
  )
    .bind(email)
    .first<User & { password_hash: string }>();

  if (!user) {
    return error("Invalid email or password", 401);
  }

  const validPassword = await verifyPassword(password, user.password_hash);
  if (!validPassword) {
    return error("Invalid email or password", 401);
  }

  const sessionId = await createSession(env, user.id);
  const maxAge = Math.floor(SESSION_DURATION_MS / 1000);

  return json({ user: { id: user.id, email: user.email, name: user.name } }, 200, {
    "Set-Cookie": setSessionCookie(sessionId, maxAge),
  });
}

async function handleLogout(env: Env, request: Request): Promise<Response> {
  const sessionId = getSessionIdFromCookie(request);

  if (sessionId) {
    await deleteSession(env, sessionId);
  }

  return json({ success: true }, 200, { "Set-Cookie": clearSessionCookie() });
}

async function handleRegister(env: Env, request: Request): Promise<Response> {
  const body = (await request.json()) as { email?: string; name?: string; password?: string };
  const { email, name, password } = body;

  if (!email || !name || !password) {
    return error("Email, name, and password are required");
  }

  if (password.length < 8) {
    return error("Password must be at least 8 characters");
  }

  const id = crypto.randomUUID();
  const { hash: passwordHash } = await hashPassword(password);

  try {
    await env.DB.prepare("INSERT INTO users (id, email, name, password_hash) VALUES (?, ?, ?, ?)")
      .bind(id, email, name, passwordHash)
      .run();

    // Auto-login after registration
    const sessionId = await createSession(env, id);
    const maxAge = Math.floor(SESSION_DURATION_MS / 1000);

    return json({ user: { id, email, name } }, 201, {
      "Set-Cookie": setSessionCookie(sessionId, maxAge),
    });
  } catch (e) {
    if ((e as Error).message.includes("UNIQUE constraint failed")) {
      return error("Email already exists", 409);
    }
    throw e;
  }
}

async function handleGetMe(env: Env, request: Request): Promise<Response> {
  const sessionId = getSessionIdFromCookie(request);

  if (!sessionId) {
    return error("Not authenticated", 401);
  }

  const user = await getSessionUser(env, sessionId);

  if (!user) {
    return json({ error: "Session expired" }, 401, { "Set-Cookie": clearSessionCookie() });
  }

  // Refresh session on activity
  await refreshSession(env, sessionId);

  return json({ user });
}

async function handleRefreshSession(env: Env, request: Request): Promise<Response> {
  const sessionId = getSessionIdFromCookie(request);

  if (!sessionId) {
    return error("Not authenticated", 401);
  }

  const user = await getSessionUser(env, sessionId);

  if (!user) {
    return json({ error: "Session expired" }, 401, { "Set-Cookie": clearSessionCookie() });
  }

  await refreshSession(env, sessionId);
  const maxAge = Math.floor(SESSION_DURATION_MS / 1000);

  return json({ user, expiresAt: new Date(Date.now() + SESSION_DURATION_MS).toISOString() }, 200, {
    "Set-Cookie": setSessionCookie(sessionId, maxAge),
  });
}

// Route handlers
async function handleGetUsers(env: Env): Promise<Response> {
  const { results } = await env.DB.prepare(
    "SELECT id, email, name, created_at FROM users ORDER BY created_at DESC LIMIT 100",
  ).all<User>();
  return json({ users: results });
}

async function handleGetUser(env: Env, id: string): Promise<Response> {
  const user = await env.DB.prepare("SELECT id, email, name, created_at FROM users WHERE id = ?")
    .bind(id)
    .first<User>();

  if (!user) {
    return error("User not found", 404);
  }
  return json({ user });
}

async function handleCreateUser(env: Env, request: Request): Promise<Response> {
  const body = (await request.json()) as { email?: string; name?: string; password?: string };
  const { email, name, password } = body;

  if (!email || !name || !password) {
    return error("Email, name, and password are required");
  }

  const id = crypto.randomUUID();
  const { hash: passwordHash } = await hashPassword(password);

  try {
    await env.DB.prepare("INSERT INTO users (id, email, name, password_hash) VALUES (?, ?, ?, ?)")
      .bind(id, email, name, passwordHash)
      .run();

    return json({ user: { id, email, name } }, 201);
  } catch (e) {
    if ((e as Error).message.includes("UNIQUE constraint failed")) {
      return error("Email already exists", 409);
    }
    throw e;
  }
}

async function handleHealthCheck(env: Env): Promise<Response> {
  // Quick DB health check
  try {
    await env.DB.prepare("SELECT 1").first();
    // Opportunistically clean up expired sessions
    await cleanupExpiredSessions(env);
    return json({
      status: "healthy",
      app: env.APP_NAME,
      timestamp: new Date().toISOString(),
    });
  } catch {
    return json({ status: "unhealthy", error: "Database unavailable" }, 503);
  }
}

// Project handlers
async function handleGetProjects(env: Env, userId: string): Promise<Response> {
  const { results } = await env.DB.prepare(
    "SELECT id, user_id, name, description, status, created_at, updated_at FROM projects WHERE user_id = ? ORDER BY created_at DESC",
  )
    .bind(userId)
    .all<Project>();
  return json({ projects: results });
}

async function handleGetProject(env: Env, id: string, userId: string): Promise<Response> {
  const project = await env.DB.prepare(
    "SELECT id, user_id, name, description, status, created_at, updated_at FROM projects WHERE id = ? AND user_id = ?",
  )
    .bind(id, userId)
    .first<Project>();

  if (!project) {
    return error("Project not found", 404);
  }
  return json({ project });
}

async function handleCreateProject(env: Env, request: Request, userId: string): Promise<Response> {
  const body = (await request.json()) as { name?: string; description?: string };
  const { name, description } = body;

  if (!name) {
    return error("Name is required");
  }

  const id = crypto.randomUUID();
  const now = new Date().toISOString();

  await env.DB.prepare(
    "INSERT INTO projects (id, user_id, name, description, status, created_at, updated_at) VALUES (?, ?, ?, ?, 'active', ?, ?)",
  )
    .bind(id, userId, name, description || null, now, now)
    .run();

  return json(
    {
      project: {
        id,
        user_id: userId,
        name,
        description: description || null,
        status: "active",
        created_at: now,
        updated_at: now,
      },
    },
    201,
  );
}

async function handleUpdateProject(
  env: Env,
  request: Request,
  id: string,
  userId: string,
): Promise<Response> {
  const existing = await env.DB.prepare("SELECT id FROM projects WHERE id = ? AND user_id = ?")
    .bind(id, userId)
    .first();

  if (!existing) {
    return error("Project not found", 404);
  }

  const body = (await request.json()) as { name?: string; description?: string; status?: string };
  const updates: string[] = [];
  const values: (string | null)[] = [];

  if (body.name !== undefined) {
    updates.push("name = ?");
    values.push(body.name);
  }
  if (body.description !== undefined) {
    updates.push("description = ?");
    values.push(body.description);
  }
  if (body.status !== undefined) {
    if (body.status !== "active" && body.status !== "archived") {
      return error("Status must be 'active' or 'archived'");
    }
    updates.push("status = ?");
    values.push(body.status);
  }

  if (updates.length === 0) {
    return error("No fields to update");
  }

  const now = new Date().toISOString();
  updates.push("updated_at = ?");
  values.push(now);
  values.push(id);
  values.push(userId);

  await env.DB.prepare(`UPDATE projects SET ${updates.join(", ")} WHERE id = ? AND user_id = ?`)
    .bind(...values)
    .run();

  const project = await env.DB.prepare(
    "SELECT id, user_id, name, description, status, created_at, updated_at FROM projects WHERE id = ?",
  )
    .bind(id)
    .first<Project>();

  return json({ project });
}

async function handleDeleteProject(env: Env, id: string, userId: string): Promise<Response> {
  const existing = await env.DB.prepare("SELECT id FROM projects WHERE id = ? AND user_id = ?")
    .bind(id, userId)
    .first();

  if (!existing) {
    return error("Project not found", 404);
  }

  await env.DB.prepare("DELETE FROM projects WHERE id = ? AND user_id = ?").bind(id, userId).run();

  return json({ success: true });
}

// Main request handler
export const onRequest: PagesFunction<Env> = async (context) => {
  const { request, env, params } = context;
  const method = request.method;

  // Parse route from catch-all parameter
  const route = (params.route as string[])?.join("/") || "";
  const path = `/api/${route}`;

  try {
    // Health check
    if (path === "/api/health" && method === "GET") {
      return handleHealthCheck(env);
    }

    // Auth endpoints
    if (path === "/api/auth/login" && method === "POST") {
      return handleLogin(env, request);
    }

    if (path === "/api/auth/logout" && method === "POST") {
      return handleLogout(env, request);
    }

    if (path === "/api/auth/register" && method === "POST") {
      return handleRegister(env, request);
    }

    if (path === "/api/auth/me" && method === "GET") {
      return handleGetMe(env, request);
    }

    if (path === "/api/auth/refresh" && method === "POST") {
      return handleRefreshSession(env, request);
    }

    // Users endpoints
    if (path === "/api/users" && method === "GET") {
      return handleGetUsers(env);
    }

    if (path === "/api/users" && method === "POST") {
      return handleCreateUser(env, request);
    }

    const userMatch = path.match(/^\/api\/users\/([^/]+)$/);
    if (userMatch && method === "GET") {
      return handleGetUser(env, userMatch[1]);
    }

    // Projects endpoints (require authentication via session cookie)
    if (path === "/api/projects" && method === "GET") {
      const user = await getAuthenticatedUser(env, request);
      if (!user) return error("Unauthorized", 401);
      return handleGetProjects(env, user.id);
    }

    if (path === "/api/projects" && method === "POST") {
      const user = await getAuthenticatedUser(env, request);
      if (!user) return error("Unauthorized", 401);
      return handleCreateProject(env, request, user.id);
    }

    const projectMatch = path.match(/^\/api\/projects\/([^/]+)$/);
    if (projectMatch) {
      const user = await getAuthenticatedUser(env, request);
      if (!user) return error("Unauthorized", 401);
      const projectId = projectMatch[1];

      if (method === "GET") {
        return handleGetProject(env, projectId, user.id);
      }
      if (method === "PUT") {
        return handleUpdateProject(env, request, projectId, user.id);
      }
      if (method === "DELETE") {
        return handleDeleteProject(env, projectId, user.id);
      }
    }

    // 404 for unknown routes
    return error("Not found", 404);
  } catch (e) {
    console.error("API error:", e);
    return error("Internal server error", 500);
  }
};
