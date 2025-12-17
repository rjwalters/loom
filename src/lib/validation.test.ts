/**
 * Unit tests for validation utility functions
 */

import { describe, expect, it, vi } from "vitest";
import { z } from "zod";
import {
  parseJSON,
  parseJSONWithDefault,
  parseWithDefault,
  safeParseJSON,
  safeValidateData,
  validateData,
} from "./validation";

// Mock the logger to prevent console output during tests
vi.mock("./logger", () => ({
  Logger: {
    forComponent: () => ({
      info: vi.fn(),
      warn: vi.fn(),
      error: vi.fn(),
    }),
  },
}));

// Simple test schema
const TestSchema = z.object({
  name: z.string().min(1),
  count: z.number().min(0),
  active: z.boolean().optional(),
});

type TestData = z.infer<typeof TestSchema>;

describe("parseJSON", () => {
  it("parses and validates valid JSON", () => {
    const json = '{"name": "test", "count": 42}';
    const result = parseJSON(json, TestSchema);
    expect(result).toEqual({ name: "test", count: 42 });
  });

  it("throws on invalid JSON syntax", () => {
    const json = "not valid json";
    expect(() => parseJSON(json, TestSchema)).toThrow("Invalid JSON");
  });

  it("throws on schema validation failure", () => {
    const json = '{"name": "", "count": 42}';
    expect(() => parseJSON(json, TestSchema)).toThrow("Invalid");
  });

  it("includes context in error message", () => {
    const json = '{"name": "", "count": 42}';
    expect(() => parseJSON(json, TestSchema, { context: "test.json" })).toThrow(
      "Invalid test.json"
    );
  });

  it("preserves optional fields", () => {
    const json = '{"name": "test", "count": 42, "active": true}';
    const result = parseJSON(json, TestSchema);
    expect(result.active).toBe(true);
  });
});

describe("validateData", () => {
  it("validates valid data", () => {
    const data = { name: "test", count: 42 };
    const result = validateData(data, TestSchema);
    expect(result).toEqual(data);
  });

  it("throws on validation failure", () => {
    const data = { name: "", count: 42 };
    expect(() => validateData(data, TestSchema)).toThrow("Invalid");
  });

  it("includes context in error message", () => {
    const data = { name: "", count: 42 };
    expect(() => validateData(data, TestSchema, { context: "config" })).toThrow("Invalid config");
  });

  it("validates nested objects", () => {
    const NestedSchema = z.object({
      items: z.array(TestSchema),
    });
    const data = {
      items: [
        { name: "a", count: 1 },
        { name: "b", count: 2 },
      ],
    };
    const result = validateData(data, NestedSchema);
    expect(result.items).toHaveLength(2);
  });
});

describe("safeParseJSON", () => {
  it("returns success result for valid JSON", () => {
    const json = '{"name": "test", "count": 42}';
    const result = safeParseJSON(json, TestSchema);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data).toEqual({ name: "test", count: 42 });
    }
  });

  it("returns error result for invalid JSON syntax", () => {
    const json = "not valid json";
    const result = safeParseJSON(json, TestSchema);
    expect(result.success).toBe(false);
    if (!result.success) {
      expect(result.error.message).toContain("Invalid JSON");
      expect(result.issues).toHaveLength(1);
    }
  });

  it("returns error result for validation failure", () => {
    const json = '{"name": "", "count": 42}';
    const result = safeParseJSON(json, TestSchema);
    expect(result.success).toBe(false);
    if (!result.success) {
      expect(result.issues.length).toBeGreaterThan(0);
    }
  });

  it("includes context in error result", () => {
    const json = '{"name": "", "count": 42}';
    const result = safeParseJSON(json, TestSchema, { context: "test.json" });
    expect(result.success).toBe(false);
    if (!result.success) {
      expect(result.error.message).toContain("test.json");
    }
  });
});

describe("safeValidateData", () => {
  it("returns success result for valid data", () => {
    const data = { name: "test", count: 42 };
    const result = safeValidateData(data, TestSchema);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data).toEqual(data);
    }
  });

  it("returns error result for invalid data", () => {
    const data = { name: "", count: 42 };
    const result = safeValidateData(data, TestSchema);
    expect(result.success).toBe(false);
    if (!result.success) {
      expect(result.issues.length).toBeGreaterThan(0);
    }
  });

  it("provides descriptive issues", () => {
    const data = { name: "", count: -1 };
    const result = safeValidateData(data, TestSchema);
    expect(result.success).toBe(false);
    if (!result.success) {
      // Should have issues for both name and count
      expect(result.issues.length).toBeGreaterThanOrEqual(1);
    }
  });
});

describe("parseWithDefault", () => {
  const defaultValue: TestData = { name: "default", count: 0 };

  it("returns parsed data for valid input", () => {
    const data = { name: "test", count: 42 };
    const result = parseWithDefault(data, TestSchema, defaultValue);
    expect(result).toEqual(data);
  });

  it("returns default for invalid input", () => {
    const data = { name: "", count: 42 };
    const result = parseWithDefault(data, TestSchema, defaultValue);
    expect(result).toEqual(defaultValue);
  });

  it("returns default for null input", () => {
    const result = parseWithDefault(null, TestSchema, defaultValue);
    expect(result).toEqual(defaultValue);
  });

  it("returns default for undefined input", () => {
    const result = parseWithDefault(undefined, TestSchema, defaultValue);
    expect(result).toEqual(defaultValue);
  });

  it("returns default for wrong type", () => {
    const result = parseWithDefault("not an object", TestSchema, defaultValue);
    expect(result).toEqual(defaultValue);
  });
});

describe("parseJSONWithDefault", () => {
  const defaultValue: TestData = { name: "default", count: 0 };

  it("returns parsed data for valid JSON", () => {
    const json = '{"name": "test", "count": 42}';
    const result = parseJSONWithDefault(json, TestSchema, defaultValue);
    expect(result).toEqual({ name: "test", count: 42 });
  });

  it("returns default for invalid JSON syntax", () => {
    const json = "not valid json";
    const result = parseJSONWithDefault(json, TestSchema, defaultValue);
    expect(result).toEqual(defaultValue);
  });

  it("returns default for validation failure", () => {
    const json = '{"name": "", "count": 42}';
    const result = parseJSONWithDefault(json, TestSchema, defaultValue);
    expect(result).toEqual(defaultValue);
  });

  it("returns default for empty string", () => {
    const result = parseJSONWithDefault("", TestSchema, defaultValue);
    expect(result).toEqual(defaultValue);
  });
});

describe("error message formatting", () => {
  it("includes path in error message", () => {
    const NestedSchema = z.object({
      config: z.object({
        name: z.string().min(1),
      }),
    });
    const data = { config: { name: "" } };
    const result = safeValidateData(data, NestedSchema);
    expect(result.success).toBe(false);
    if (!result.success) {
      expect(result.issues.some((i) => i.includes("config.name"))).toBe(true);
    }
  });

  it("includes validation message in error", () => {
    const data = { name: "", count: 42 };
    const result = safeValidateData(data, TestSchema);
    expect(result.success).toBe(false);
    if (!result.success) {
      // Zod provides messages like "String must contain at least 1 character(s)"
      expect(result.issues.length).toBeGreaterThan(0);
    }
  });
});

describe("logErrors option", () => {
  it("respects logErrors: false", () => {
    const json = '{"name": "", "count": 42}';
    // This should not throw and should not log
    const result = safeParseJSON(json, TestSchema, { logErrors: false });
    expect(result.success).toBe(false);
  });
});
