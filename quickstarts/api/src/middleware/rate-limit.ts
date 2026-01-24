import type { MiddlewareHandler } from "hono";
import { HTTPException } from "hono/http-exception";
import type { Env } from "../types";

interface RateLimitConfig {
  windowMs: number; // Time window in milliseconds
  maxRequests: number; // Max requests per window
}

const DEFAULT_CONFIG: RateLimitConfig = {
  windowMs: 60 * 1000, // 1 minute
  maxRequests: 100, // 100 requests per minute
};

/**
 * Rate limiter using Cloudflare KV for distributed state
 * Falls back to allowing requests if KV is unavailable
 */
export const rateLimiter: MiddlewareHandler<{ Bindings: Env }> = async (c, next) => {
  const config = DEFAULT_CONFIG;
  const ip = c.req.header("CF-Connecting-IP") || c.req.header("X-Forwarded-For") || "unknown";
  const key = `ratelimit:${ip}`;
  const now = Date.now();
  const windowStart = now - config.windowMs;

  try {
    // Get current rate limit data from KV
    const data = await c.env.KV.get(key, "json") as { requests: number[]; } | null;
    const requests = data?.requests?.filter((t) => t > windowStart) || [];

    if (requests.length >= config.maxRequests) {
      throw new HTTPException(429, {
        message: "Too many requests. Please try again later.",
      });
    }

    // Add current request timestamp
    requests.push(now);

    // Store updated data with TTL
    await c.env.KV.put(key, JSON.stringify({ requests }), {
      expirationTtl: Math.ceil(config.windowMs / 1000),
    });

    // Add rate limit headers
    c.header("X-RateLimit-Limit", config.maxRequests.toString());
    c.header("X-RateLimit-Remaining", (config.maxRequests - requests.length).toString());
    c.header("X-RateLimit-Reset", (now + config.windowMs).toString());
  } catch (err) {
    if (err instanceof HTTPException) throw err;
    // If KV fails, allow the request but log the error
    console.error("[RateLimit] KV error:", err);
  }

  await next();
};
