/**
 * Validation utilities for safe JSON parsing with Zod schema validation.
 * Provides consistent error handling and logging for data validation.
 *
 * @module validation
 */

import type { z } from "zod";
import { Logger } from "./logger";

const logger = Logger.forComponent("validation");

/**
 * Options for validation functions
 */
export interface ValidationOptions {
  /** Context string for error messages (e.g., "config.json", "role metadata") */
  context?: string;
  /** Whether to log validation errors (default: true) */
  logErrors?: boolean;
}

/**
 * Result type for safe validation operations
 */
export type ValidationResult<T> =
  | { success: true; data: T }
  | { success: false; error: Error; issues: string[] };

/**
 * Formats Zod validation errors into human-readable messages
 */
function formatZodError(error: z.ZodError): string[] {
  return error.issues.map((issue) => {
    const path = issue.path.length > 0 ? `${issue.path.join(".")}: ` : "";
    return `${path}${issue.message}`;
  });
}

/**
 * Safely parses JSON with schema validation.
 * Throws a descriptive error if parsing or validation fails.
 *
 * @param jsonString - The JSON string to parse
 * @param schema - The Zod schema to validate against
 * @param options - Optional validation options
 * @returns The validated and typed data
 * @throws Error with descriptive message if validation fails
 *
 * @example
 * ```ts
 * const config = parseJSON(configJson, LoomConfigSchema, {
 *   context: "config.json"
 * });
 * ```
 */
export function parseJSON<T>(
  jsonString: string,
  schema: z.ZodType<T>,
  options: ValidationOptions = {}
): T {
  const { context = "data", logErrors = true } = options;

  let raw: unknown;
  try {
    raw = JSON.parse(jsonString);
  } catch (error) {
    const message = `Invalid JSON in ${context}: ${error instanceof Error ? error.message : String(error)}`;
    if (logErrors) {
      logger.error("JSON parse failed", new Error(message), { context });
    }
    throw new Error(message);
  }

  return validateData(raw, schema, options);
}

/**
 * Validates unknown data against a Zod schema.
 * Throws a descriptive error if validation fails.
 *
 * @param data - The unknown data to validate
 * @param schema - The Zod schema to validate against
 * @param options - Optional validation options
 * @returns The validated and typed data
 * @throws Error with descriptive message if validation fails
 *
 * @example
 * ```ts
 * const state = validateData(rawData, LoomStateSchema, {
 *   context: "state.json"
 * });
 * ```
 */
export function validateData<T>(
  data: unknown,
  schema: z.ZodType<T>,
  options: ValidationOptions = {}
): T {
  const { context = "data", logErrors = true } = options;

  const result = schema.safeParse(data);

  if (!result.success) {
    const issues = formatZodError(result.error);
    const message = `Invalid ${context}: ${issues[0] || "validation failed"}`;

    if (logErrors) {
      logger.error("Schema validation failed", new Error(message), {
        context,
        issues,
        data: typeof data === "object" ? JSON.stringify(data).slice(0, 500) : String(data),
      });
    }

    throw new Error(message);
  }

  return result.data;
}

/**
 * Safely parses JSON with schema validation, returning a result object.
 * Never throws - returns success/error state instead.
 *
 * @param jsonString - The JSON string to parse
 * @param schema - The Zod schema to validate against
 * @param options - Optional validation options
 * @returns A result object with either the validated data or error information
 *
 * @example
 * ```ts
 * const result = safeParseJSON(configJson, LoomConfigSchema, {
 *   context: "config.json"
 * });
 *
 * if (result.success) {
 *   // Use result.data
 * } else {
 *   // Handle result.error, result.issues
 * }
 * ```
 */
export function safeParseJSON<T>(
  jsonString: string,
  schema: z.ZodType<T>,
  options: ValidationOptions = {}
): ValidationResult<T> {
  const { context = "data", logErrors = true } = options;

  let raw: unknown;
  try {
    raw = JSON.parse(jsonString);
  } catch (error) {
    const message = `Invalid JSON in ${context}: ${error instanceof Error ? error.message : String(error)}`;
    if (logErrors) {
      logger.error("JSON parse failed", new Error(message), { context });
    }
    return {
      success: false,
      error: new Error(message),
      issues: [message],
    };
  }

  return safeValidateData(raw, schema, options);
}

/**
 * Safely validates unknown data against a Zod schema, returning a result object.
 * Never throws - returns success/error state instead.
 *
 * @param data - The unknown data to validate
 * @param schema - The Zod schema to validate against
 * @param options - Optional validation options
 * @returns A result object with either the validated data or error information
 *
 * @example
 * ```ts
 * const result = safeValidateData(rawData, LoomStateSchema, {
 *   context: "state.json"
 * });
 *
 * if (result.success) {
 *   // Use result.data
 * } else {
 *   // Handle result.error, result.issues
 * }
 * ```
 */
export function safeValidateData<T>(
  data: unknown,
  schema: z.ZodType<T>,
  options: ValidationOptions = {}
): ValidationResult<T> {
  const { context = "data", logErrors = true } = options;

  const result = schema.safeParse(data);

  if (!result.success) {
    const issues = formatZodError(result.error);
    const message = `Invalid ${context}: ${issues[0] || "validation failed"}`;

    if (logErrors) {
      logger.error("Schema validation failed", new Error(message), {
        context,
        issues,
      });
    }

    return {
      success: false,
      error: new Error(message),
      issues,
    };
  }

  return {
    success: true,
    data: result.data,
  };
}

/**
 * Parses and validates data, returning a default value on failure.
 * Logs validation errors as warnings but continues with the fallback.
 *
 * @param data - The unknown data to validate
 * @param schema - The Zod schema to validate against
 * @param defaultValue - The default value to return on failure
 * @param options - Optional validation options
 * @returns The validated data or the default value
 *
 * @example
 * ```ts
 * const config = parseWithDefault(
 *   rawConfig,
 *   LoomConfigSchema,
 *   { version: "2", terminals: [] },
 *   { context: "config.json" }
 * );
 * ```
 */
export function parseWithDefault<T>(
  data: unknown,
  schema: z.ZodType<T>,
  defaultValue: T,
  options: ValidationOptions = {}
): T {
  const { context = "data" } = options;

  const result = schema.safeParse(data);

  if (!result.success) {
    const issues = formatZodError(result.error);
    logger.warn("Validation failed, using default", {
      context,
      issues,
    });
    return defaultValue;
  }

  return result.data;
}

/**
 * Parses JSON and validates, returning a default value on failure.
 * Logs validation errors as warnings but continues with the fallback.
 *
 * @param jsonString - The JSON string to parse
 * @param schema - The Zod schema to validate against
 * @param defaultValue - The default value to return on failure
 * @param options - Optional validation options
 * @returns The validated data or the default value
 *
 * @example
 * ```ts
 * const metadata = parseJSONWithDefault(
 *   metadataJson,
 *   RoleMetadataSchema,
 *   {},
 *   { context: "role metadata" }
 * );
 * ```
 */
export function parseJSONWithDefault<T>(
  jsonString: string,
  schema: z.ZodType<T>,
  defaultValue: T,
  options: ValidationOptions = {}
): T {
  const { context = "data" } = options;

  let raw: unknown;
  try {
    raw = JSON.parse(jsonString);
  } catch (error) {
    logger.warn("JSON parse failed, using default", {
      context,
      error: error instanceof Error ? error.message : String(error),
    });
    return defaultValue;
  }

  return parseWithDefault(raw, schema, defaultValue, options);
}
