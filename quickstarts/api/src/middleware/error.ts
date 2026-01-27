import type { ErrorHandler } from "hono";
import { HTTPException } from "hono/http-exception";
import { ZodError } from "zod";

export interface ApiError {
  error: string;
  message: string;
  details?: unknown;
}

export const errorHandler: ErrorHandler = (err, c) => {
  console.error(`[Error] ${err.message}`, err.stack);

  // Handle Zod validation errors
  if (err instanceof ZodError) {
    return c.json<ApiError>(
      {
        error: "Validation Error",
        message: "Invalid request data",
        details: err.errors.map((e) => ({
          path: e.path.join("."),
          message: e.message,
        })),
      },
      400,
    );
  }

  // Handle HTTP exceptions
  if (err instanceof HTTPException) {
    return c.json<ApiError>(
      {
        error: err.message,
        message: err.message,
      },
      err.status,
    );
  }

  // Handle generic errors
  return c.json<ApiError>(
    {
      error: "Internal Server Error",
      message: "An unexpected error occurred",
    },
    500,
  );
};
