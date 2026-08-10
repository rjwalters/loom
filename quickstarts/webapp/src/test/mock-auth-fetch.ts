import { type Mock, vi } from "vitest";

export interface MockAuthUser {
  id: string;
  email: string;
  name: string;
}

export interface MockAuthFetchFailure {
  /** Which auth endpoint should respond with an error. */
  endpoint: "me" | "login" | "register" | "logout";
  status?: number;
  error?: string;
}

export interface MockAuthFetchOptions {
  /** The "already authenticated" user returned by GET /api/auth/me. Defaults to unauthenticated. */
  user?: MockAuthUser | null;
  /** Force one endpoint to fail with a given status/message instead of succeeding. */
  failWith?: MockAuthFetchFailure;
  /** Artificial network delay (ms) before resolving, so in-flight/loading UI is observable. */
  delayMs?: number;
}

function jsonResponse(data: unknown, status = 200): Response {
  return new Response(JSON.stringify(data), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

function parseBody(init?: RequestInit): Record<string, string | undefined> {
  if (!init?.body) return {};
  try {
    return JSON.parse(String(init.body));
  } catch {
    return {};
  }
}

/**
 * Stubs `global.fetch` to emulate the cookie/session-backed `/api/auth/*`
 * endpoints consumed by `useAuth` (see `src/hooks/use-auth.tsx`), so tests
 * can exercise the real hook/component code path instead of a nonexistent
 * localStorage-backed implementation.
 */
export function mockAuthFetch({
  user = null,
  failWith,
  delayMs = 0,
}: MockAuthFetchOptions = {}): Mock {
  let current: MockAuthUser | null = user;

  const spy = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    if (delayMs > 0) {
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }

    const url = String(input);

    if (url.endsWith("/api/auth/me")) {
      if (failWith?.endpoint === "me") {
        return jsonResponse({ error: failWith.error ?? "Unauthorized" }, failWith.status ?? 401);
      }
      return current
        ? jsonResponse({ user: current })
        : jsonResponse({ error: "Not authenticated" }, 401);
    }

    if (url.endsWith("/api/auth/login")) {
      if (failWith?.endpoint === "login") {
        return jsonResponse(
          { error: failWith.error ?? "Invalid email or password" },
          failWith.status ?? 401,
        );
      }
      const body = parseBody(init);
      if (!body.email || !body.password) {
        return jsonResponse({ error: "Email and password are required" }, 400);
      }
      current = { id: "mock-user-id", email: body.email, name: body.email.split("@")[0] };
      return jsonResponse({ user: current });
    }

    if (url.endsWith("/api/auth/register")) {
      if (failWith?.endpoint === "register") {
        return jsonResponse(
          { error: failWith.error ?? "Registration failed" },
          failWith.status ?? 400,
        );
      }
      const body = parseBody(init);
      if (!body.email || !body.password || !body.name) {
        return jsonResponse({ error: "Email, name, and password are required" }, 400);
      }
      current = { id: "mock-user-id", email: body.email, name: body.name };
      return jsonResponse({ user: current }, 201);
    }

    if (url.endsWith("/api/auth/logout")) {
      if (failWith?.endpoint === "logout") {
        return jsonResponse({ error: failWith.error ?? "Logout failed" }, failWith.status ?? 400);
      }
      current = null;
      return jsonResponse({ success: true });
    }

    throw new Error(`mockAuthFetch: unhandled request to ${url}`);
  });

  vi.stubGlobal("fetch", spy);
  return spy;
}
