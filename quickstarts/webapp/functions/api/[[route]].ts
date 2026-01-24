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

// Simple response helpers
function json(data: unknown, status = 200) {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function error(message: string, status = 400) {
  return json({ error: message }, status);
}

// Extract user ID from X-User-Id header (demo auth - in production use proper session/JWT)
function getUserId(request: Request): string | null {
  return request.headers.get("X-User-Id");
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
  // In production, use proper password hashing (e.g., bcrypt via a Worker)
  const passwordHash = await hashPassword(password);

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

// Simple password hashing (for demo - use bcrypt in production)
async function hashPassword(password: string): Promise<string> {
  const encoder = new TextEncoder();
  const data = encoder.encode(`${password}loom-quickstart-salt`);
  const hash = await crypto.subtle.digest("SHA-256", data);
  return btoa(String.fromCharCode(...new Uint8Array(hash)));
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

    // Projects endpoints (require authentication)
    if (path === "/api/projects" && method === "GET") {
      const userId = getUserId(request);
      if (!userId) return error("Unauthorized", 401);
      return handleGetProjects(env, userId);
    }

    if (path === "/api/projects" && method === "POST") {
      const userId = getUserId(request);
      if (!userId) return error("Unauthorized", 401);
      return handleCreateProject(env, request, userId);
    }

    const projectMatch = path.match(/^\/api\/projects\/([^/]+)$/);
    if (projectMatch) {
      const userId = getUserId(request);
      if (!userId) return error("Unauthorized", 401);
      const projectId = projectMatch[1];

      if (method === "GET") {
        return handleGetProject(env, projectId, userId);
      }
      if (method === "PUT") {
        return handleUpdateProject(env, request, projectId, userId);
      }
      if (method === "DELETE") {
        return handleDeleteProject(env, projectId, userId);
      }
    }

    // 404 for unknown routes
    return error("Not found", 404);
  } catch (e) {
    console.error("API error:", e);
    return error("Internal server error", 500);
  }
};
