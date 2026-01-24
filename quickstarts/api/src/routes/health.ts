import { OpenAPIHono, createRoute, z } from "@hono/zod-openapi";
import type { Env } from "../types";

export const healthRoutes = new OpenAPIHono<{ Bindings: Env }>();

const healthRoute = createRoute({
  method: "get",
  path: "/health",
  tags: ["Health"],
  summary: "Health check endpoint",
  responses: {
    200: {
      description: "Service is healthy",
      content: {
        "application/json": {
          schema: z.object({
            status: z.string(),
            app: z.string(),
            timestamp: z.string(),
            database: z.string(),
          }),
        },
      },
    },
    503: {
      description: "Service is unhealthy",
      content: {
        "application/json": {
          schema: z.object({
            status: z.string(),
            error: z.string(),
          }),
        },
      },
    },
  },
});

healthRoutes.openapi(healthRoute, async (c) => {
  try {
    // Quick database health check
    await c.env.DB.prepare("SELECT 1").first();

    return c.json({
      status: "healthy",
      app: c.env.APP_NAME,
      timestamp: new Date().toISOString(),
      database: "connected",
    });
  } catch {
    return c.json(
      {
        status: "unhealthy",
        error: "Database unavailable",
      },
      503
    );
  }
});
