import type { MiddlewareHandler } from "hono";
import { HTTPException } from "hono/http-exception";
import type { Env, JwtPayload } from "../types";
import { verifyJwt } from "../lib/jwt";

declare module "hono" {
  interface ContextVariableMap {
    user: JwtPayload;
  }
}

/**
 * Authentication middleware - validates JWT token
 */
export const requireAuth: MiddlewareHandler<{ Bindings: Env }> = async (c, next) => {
  const authHeader = c.req.header("Authorization");

  if (!authHeader || !authHeader.startsWith("Bearer ")) {
    throw new HTTPException(401, { message: "Missing or invalid authorization header" });
  }

  const token = authHeader.substring(7);

  try {
    const payload = await verifyJwt(token, c.env.JWT_SECRET);
    c.set("user", payload);
    await next();
  } catch {
    throw new HTTPException(401, { message: "Invalid or expired token" });
  }
};

/**
 * Role-based access control middleware
 */
export const requireRole = (
  ...allowedRoles: string[]
): MiddlewareHandler<{ Bindings: Env }> => {
  return async (c, next) => {
    const user = c.get("user");

    if (!user) {
      throw new HTTPException(401, { message: "Authentication required" });
    }

    if (!allowedRoles.includes(user.role)) {
      throw new HTTPException(403, { message: "Insufficient permissions" });
    }

    await next();
  };
};

/**
 * Optional auth - sets user if token present, continues otherwise
 */
export const optionalAuth: MiddlewareHandler<{ Bindings: Env }> = async (c, next) => {
  const authHeader = c.req.header("Authorization");

  if (authHeader?.startsWith("Bearer ")) {
    const token = authHeader.substring(7);
    try {
      const payload = await verifyJwt(token, c.env.JWT_SECRET);
      c.set("user", payload);
    } catch {
      // Token invalid, continue without user
    }
  }

  await next();
};
