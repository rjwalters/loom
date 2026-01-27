import { createRoute, OpenAPIHono, z } from "@hono/zod-openapi";
import { HTTPException } from "hono/http-exception";
import { requireAuth, requireRole } from "../middleware/auth";
import { ErrorSchema } from "../schemas/auth";
import {
  UpdateUserSchema,
  UserListSchema,
  UserParamsSchema,
  UserSchema,
} from "../schemas/user";
import type { Env, User } from "../types";

export const userRoutes = new OpenAPIHono<{ Bindings: Env }>();

// Apply auth middleware to all routes
userRoutes.use("*", requireAuth);

// GET /users - List all users (admin only)
const listUsersRoute = createRoute({
  method: "get",
  path: "/",
  tags: ["Users"],
  summary: "List all users",
  security: [{ bearerAuth: [] }],
  request: {
    query: z.object({
      limit: z.string().optional().default("50"),
      offset: z.string().optional().default("0"),
    }),
  },
  responses: {
    200: {
      description: "List of users",
      content: {
        "application/json": {
          schema: UserListSchema,
        },
      },
    },
    403: {
      description: "Forbidden - Admin only",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
  },
});

userRoutes.openapi(listUsersRoute, requireRole("admin"), async (c) => {
  const { limit, offset } = c.req.valid("query");

  const [countResult, usersResult] = await Promise.all([
    c.env.DB.prepare("SELECT COUNT(*) as count FROM users").first<{
      count: number;
    }>(),
    c.env.DB.prepare(
      "SELECT id, email, name, role, created_at, updated_at FROM users ORDER BY created_at DESC LIMIT ? OFFSET ?",
    )
      .bind(parseInt(limit, 10), parseInt(offset, 10))
      .all<User>(),
  ]);

  return c.json({
    users: usersResult.results,
    total: countResult?.count || 0,
  });
});

// GET /users/:id - Get user by ID
const getUserRoute = createRoute({
  method: "get",
  path: "/{id}",
  tags: ["Users"],
  summary: "Get user by ID",
  security: [{ bearerAuth: [] }],
  request: {
    params: UserParamsSchema,
  },
  responses: {
    200: {
      description: "User details",
      content: {
        "application/json": {
          schema: z.object({ user: UserSchema }),
        },
      },
    },
    404: {
      description: "User not found",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
  },
});

userRoutes.openapi(getUserRoute, async (c) => {
  const { id } = c.req.valid("param");
  const currentUser = c.get("user");

  // Users can only view their own profile unless admin
  if (currentUser.sub !== id && currentUser.role !== "admin") {
    throw new HTTPException(403, { message: "Cannot view other users" });
  }

  const user = await c.env.DB.prepare(
    "SELECT id, email, name, role, created_at, updated_at FROM users WHERE id = ?",
  )
    .bind(id)
    .first<User>();

  if (!user) {
    throw new HTTPException(404, { message: "User not found" });
  }

  return c.json({ user });
});

// PUT /users/:id - Update user
const updateUserRoute = createRoute({
  method: "put",
  path: "/{id}",
  tags: ["Users"],
  summary: "Update user",
  security: [{ bearerAuth: [] }],
  request: {
    params: UserParamsSchema,
    body: {
      content: {
        "application/json": {
          schema: UpdateUserSchema,
        },
      },
    },
  },
  responses: {
    200: {
      description: "User updated",
      content: {
        "application/json": {
          schema: z.object({ user: UserSchema }),
        },
      },
    },
    403: {
      description: "Forbidden",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
    404: {
      description: "User not found",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
  },
});

userRoutes.openapi(updateUserRoute, async (c) => {
  const { id } = c.req.valid("param");
  const updates = c.req.valid("json");
  const currentUser = c.get("user");

  // Users can only update their own profile unless admin
  if (currentUser.sub !== id && currentUser.role !== "admin") {
    throw new HTTPException(403, { message: "Cannot update other users" });
  }

  // Build dynamic update query
  const fields: string[] = [];
  const values: (string | undefined)[] = [];

  if (updates.name !== undefined) {
    fields.push("name = ?");
    values.push(updates.name);
  }
  if (updates.email !== undefined) {
    fields.push("email = ?");
    values.push(updates.email);
  }

  if (fields.length === 0) {
    throw new HTTPException(400, { message: "No fields to update" });
  }

  fields.push("updated_at = datetime('now')");
  values.push(id);

  await c.env.DB.prepare(`UPDATE users SET ${fields.join(", ")} WHERE id = ?`)
    .bind(...values)
    .run();

  const user = await c.env.DB.prepare(
    "SELECT id, email, name, role, created_at, updated_at FROM users WHERE id = ?",
  )
    .bind(id)
    .first<User>();

  if (!user) {
    throw new HTTPException(404, { message: "User not found" });
  }

  return c.json({ user });
});

// DELETE /users/:id - Delete user (admin only)
const deleteUserRoute = createRoute({
  method: "delete",
  path: "/{id}",
  tags: ["Users"],
  summary: "Delete user",
  security: [{ bearerAuth: [] }],
  request: {
    params: UserParamsSchema,
  },
  responses: {
    200: {
      description: "User deleted",
      content: {
        "application/json": {
          schema: z.object({ success: z.boolean() }),
        },
      },
    },
    403: {
      description: "Forbidden - Admin only",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
    404: {
      description: "User not found",
      content: {
        "application/json": {
          schema: ErrorSchema,
        },
      },
    },
  },
});

userRoutes.openapi(deleteUserRoute, requireRole("admin"), async (c) => {
  const { id } = c.req.valid("param");

  const result = await c.env.DB.prepare("DELETE FROM users WHERE id = ?")
    .bind(id)
    .run();

  if (result.meta.changes === 0) {
    throw new HTTPException(404, { message: "User not found" });
  }

  return c.json({ success: true });
});
