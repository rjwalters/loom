import { swaggerUI } from "@hono/swagger-ui";
import { OpenAPIHono } from "@hono/zod-openapi";
import { cors } from "hono/cors";
import { logger } from "hono/logger";
import { secureHeaders } from "hono/secure-headers";
import { errorHandler } from "./middleware/error";
import { rateLimiter } from "./middleware/rate-limit";
import { authRoutes } from "./routes/auth";
import { healthRoutes } from "./routes/health";
import { userRoutes } from "./routes/users";
import type { Env } from "./types";

const app = new OpenAPIHono<{ Bindings: Env }>();

// Global middleware
app.use("*", logger());
app.use("*", secureHeaders());
app.use(
  "*",
  cors({
    origin: ["http://localhost:3000", "http://localhost:5173"],
    allowMethods: ["GET", "POST", "PUT", "DELETE", "OPTIONS"],
    allowHeaders: ["Content-Type", "Authorization"],
    credentials: true,
  }),
);

// Rate limiting on API routes
app.use("/api/*", rateLimiter);

// Error handling
app.onError(errorHandler);

// Mount routes
app.route("/", healthRoutes);
app.route("/api/auth", authRoutes);
app.route("/api/users", userRoutes);

// OpenAPI documentation
app.doc("/openapi.json", {
  openapi: "3.0.0",
  info: {
    title: "Loom Quickstart API",
    version: "0.1.0",
    description: "A RESTful API template built with Hono on Cloudflare Workers",
  },
  servers: [{ url: "http://localhost:8787", description: "Development" }],
});

// Swagger UI
app.get(
  "/docs",
  swaggerUI({
    url: "/openapi.json",
  }),
);

// Root redirect to docs
app.get("/", (c) => c.redirect("/docs"));

export default app;
