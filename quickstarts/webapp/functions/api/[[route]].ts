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

// Route handlers
async function handleGetUsers(env: Env): Promise<Response> {
  const { results } = await env.DB.prepare(
    "SELECT id, email, name, created_at FROM users ORDER BY created_at DESC LIMIT 100"
  ).all<User>();
  return json({ users: results });
}

async function handleGetUser(env: Env, id: string): Promise<Response> {
  const user = await env.DB.prepare(
    "SELECT id, email, name, created_at FROM users WHERE id = ?"
  )
    .bind(id)
    .first<User>();

  if (!user) {
    return error("User not found", 404);
  }
  return json({ user });
}

async function handleCreateUser(env: Env, request: Request): Promise<Response> {
  const body = await request.json() as { email?: string; name?: string; password?: string };
  const { email, name, password } = body;

  if (!email || !name || !password) {
    return error("Email, name, and password are required");
  }

  const id = crypto.randomUUID();
  // In production, use proper password hashing (e.g., bcrypt via a Worker)
  const passwordHash = await hashPassword(password);

  try {
    await env.DB.prepare(
      "INSERT INTO users (id, email, name, password_hash) VALUES (?, ?, ?, ?)"
    )
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

// Simple password hashing (for demo - use bcrypt in production)
async function hashPassword(password: string): Promise<string> {
  const encoder = new TextEncoder();
  const data = encoder.encode(password + "loom-quickstart-salt");
  const hash = await crypto.subtle.digest("SHA-256", data);
  return btoa(String.fromCharCode(...new Uint8Array(hash)));
}

// Main request handler
export const onRequest: PagesFunction<Env> = async (context) => {
  const { request, env, params } = context;
  const url = new URL(request.url);
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

    // 404 for unknown routes
    return error("Not found", 404);
  } catch (e) {
    console.error("API error:", e);
    return error("Internal server error", 500);
  }
};
