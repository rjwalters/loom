import { OpenAPIHono, createRoute } from "@hono/zod-openapi";
import { HTTPException } from "hono/http-exception";
import type { Env, User } from "../types";
import { createJwt } from "../lib/jwt";
import { hashPassword, verifyPassword } from "../lib/password";
import {
  LoginSchema,
  RegisterSchema,
  AuthResponseSchema,
  ErrorSchema,
} from "../schemas/auth";

export const authRoutes = new OpenAPIHono<{ Bindings: Env }>();

// POST /auth/register
const registerRoute = createRoute({
  method: "post",
  path: "/register",
  tags: ["Auth"],
  summary: "Register a new user",
  request: {
    body: {
      content: {
        "application/json": {
          schema: RegisterSchema,
        },
      },
    },
  },
  responses: {
    201: {
      description: "User registered successfully",
      content: {
        "application/json": {
          schema: AuthResponseSchema,
        },
      },
    },
    400: {
      description: "Validation error",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
    409: {
      description: "Email already exists",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
  },
});

authRoutes.openapi(registerRoute, async (c) => {
  const { email, password, name } = c.req.valid("json");

  // Hash password
  const { hash: passwordHash } = await hashPassword(password);
  const id = crypto.randomUUID();

  try {
    await c.env.DB.prepare(
      "INSERT INTO users (id, email, name, password_hash, role) VALUES (?, ?, ?, ?, ?)"
    )
      .bind(id, email, name, passwordHash, "user")
      .run();

    // Generate JWT
    const accessToken = await createJwt(
      { sub: id, email, role: "user" },
      c.env.JWT_SECRET,
      c.env.JWT_EXPIRES_IN
    );

    const expiresAt = new Date(
      Date.now() + parseExpirationMs(c.env.JWT_EXPIRES_IN)
    ).toISOString();

    return c.json(
      {
        user: { id, email, name, role: "user" },
        accessToken,
        expiresAt,
      },
      201
    );
  } catch (e) {
    if ((e as Error).message.includes("UNIQUE constraint failed")) {
      throw new HTTPException(409, { message: "Email already exists" });
    }
    throw e;
  }
});

// POST /auth/login
const loginRoute = createRoute({
  method: "post",
  path: "/login",
  tags: ["Auth"],
  summary: "Login with email and password",
  request: {
    body: {
      content: {
        "application/json": {
          schema: LoginSchema,
        },
      },
    },
  },
  responses: {
    200: {
      description: "Login successful",
      content: {
        "application/json": {
          schema: AuthResponseSchema,
        },
      },
    },
    401: {
      description: "Invalid credentials",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
  },
});

authRoutes.openapi(loginRoute, async (c) => {
  const { email, password } = c.req.valid("json");

  const user = await c.env.DB.prepare(
    "SELECT id, email, name, password_hash, role FROM users WHERE email = ?"
  )
    .bind(email)
    .first<User & { password_hash: string }>();

  if (!user) {
    throw new HTTPException(401, { message: "Invalid email or password" });
  }

  const validPassword = await verifyPassword(password, user.password_hash);
  if (!validPassword) {
    throw new HTTPException(401, { message: "Invalid email or password" });
  }

  // Generate JWT
  const accessToken = await createJwt(
    { sub: user.id, email: user.email, role: user.role },
    c.env.JWT_SECRET,
    c.env.JWT_EXPIRES_IN
  );

  const expiresAt = new Date(
    Date.now() + parseExpirationMs(c.env.JWT_EXPIRES_IN)
  ).toISOString();

  return c.json({
    user: { id: user.id, email: user.email, name: user.name, role: user.role },
    accessToken,
    expiresAt,
  });
});

// POST /auth/refresh
const refreshRoute = createRoute({
  method: "post",
  path: "/refresh",
  tags: ["Auth"],
  summary: "Refresh access token",
  security: [{ bearerAuth: [] }],
  responses: {
    200: {
      description: "Token refreshed successfully",
      content: {
        "application/json": {
          schema: AuthResponseSchema,
        },
      },
    },
    401: {
      description: "Invalid or expired token",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
  },
});

authRoutes.openapi(refreshRoute, async (c) => {
  // For now, just require a valid auth header to refresh
  // In production, you'd use refresh tokens stored in KV
  const authHeader = c.req.header("Authorization");
  if (!authHeader?.startsWith("Bearer ")) {
    throw new HTTPException(401, { message: "Authorization required" });
  }

  const token = authHeader.substring(7);
  const { verifyJwt } = await import("../lib/jwt");

  try {
    const payload = await verifyJwt(token, c.env.JWT_SECRET);

    // Generate new token
    const accessToken = await createJwt(
      { sub: payload.sub, email: payload.email, role: payload.role },
      c.env.JWT_SECRET,
      c.env.JWT_EXPIRES_IN
    );

    const expiresAt = new Date(
      Date.now() + parseExpirationMs(c.env.JWT_EXPIRES_IN)
    ).toISOString();

    return c.json({
      user: {
        id: payload.sub,
        email: payload.email,
        name: "", // Would fetch from DB in production
        role: payload.role,
      },
      accessToken,
      expiresAt,
    });
  } catch {
    throw new HTTPException(401, { message: "Invalid or expired token" });
  }
});

function parseExpirationMs(exp: string): number {
  const match = exp.match(/^(\d+)([smhd])$/);
  if (!match) return 7 * 24 * 60 * 60 * 1000; // Default 7 days

  const value = parseInt(match[1], 10);
  const unit = match[2];

  switch (unit) {
    case "s":
      return value * 1000;
    case "m":
      return value * 60 * 1000;
    case "h":
      return value * 60 * 60 * 1000;
    case "d":
      return value * 24 * 60 * 60 * 1000;
    default:
      return 7 * 24 * 60 * 60 * 1000;
  }
}
