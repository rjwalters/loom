import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { type LogContext, type LogEntry, Logger } from "./logger";

describe("Logger", () => {
  let consoleLogSpy: ReturnType<typeof vi.spyOn>;
  let consoleWarnSpy: ReturnType<typeof vi.spyOn>;
  let consoleErrorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    // Spy on console methods
    consoleLogSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    consoleWarnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    consoleErrorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
  });

  afterEach(() => {
    // Restore console methods
    consoleLogSpy.mockRestore();
    consoleWarnSpy.mockRestore();
    consoleErrorSpy.mockRestore();
  });

  describe("forComponent", () => {
    it("creates logger instance for component", () => {
      const logger = Logger.forComponent("test-component");
      expect(logger).toBeInstanceOf(Logger);
    });

    it("sets component name correctly", () => {
      const logger = Logger.forComponent("my-component");
      logger.info("test message");

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);
      expect(entry.context.component).toBe("my-component");
    });
  });

  describe("info", () => {
    it("logs info message with correct level", () => {
      const logger = Logger.forComponent("test");
      logger.info("Info message");

      expect(consoleLogSpy).toHaveBeenCalledOnce();
      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry.level).toBe("INFO");
      expect(entry.message).toBe("Info message");
    });

    it("includes component in context", () => {
      const logger = Logger.forComponent("test-component");
      logger.info("test");

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry.context.component).toBe("test-component");
    });

    it("includes additional context", () => {
      const logger = Logger.forComponent("test");
      logger.info("test", { terminalId: "terminal-1", workspacePath: "/path" });

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry.context.terminalId).toBe("terminal-1");
      expect(entry.context.workspacePath).toBe("/path");
    });

    it("includes timestamp in ISO format", () => {
      const logger = Logger.forComponent("test");
      logger.info("test");

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry.timestamp).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}/);
      expect(() => new Date(entry.timestamp)).not.toThrow();
    });
  });

  describe("warn", () => {
    it("logs warning message with correct level", () => {
      const logger = Logger.forComponent("test");
      logger.warn("Warning message");

      expect(consoleWarnSpy).toHaveBeenCalledOnce();
      const loggedJson = consoleWarnSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry.level).toBe("WARN");
      expect(entry.message).toBe("Warning message");
    });

    it("includes component and context", () => {
      const logger = Logger.forComponent("test-component");
      logger.warn("warning", { terminalId: "terminal-2" });

      const loggedJson = consoleWarnSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry.context.component).toBe("test-component");
      expect(entry.context.terminalId).toBe("terminal-2");
    });
  });

  describe("error", () => {
    it("logs error message with Error object", () => {
      const logger = Logger.forComponent("test");
      const error = new Error("Test error");
      logger.error("Error occurred", error);

      expect(consoleErrorSpy).toHaveBeenCalledOnce();
      const loggedJson = consoleErrorSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry.level).toBe("ERROR");
      expect(entry.message).toBe("Error occurred");
    });

    it("includes error message and stack for Error objects", () => {
      const logger = Logger.forComponent("test");
      const error = new Error("Test error");
      logger.error("Failed", error);

      const loggedJson = consoleErrorSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);
      const context = entry.context as LogContext & { errorMessage?: string; errorStack?: string };

      expect(context.errorMessage).toBe("Test error");
      expect(context.errorStack).toBeDefined();
      expect(context.errorStack).toContain("Error: Test error");
    });

    it("generates unique error ID", () => {
      const logger = Logger.forComponent("test");
      const error = new Error("Test");
      logger.error("Error 1", error);
      logger.error("Error 2", error);

      const log1Json = consoleErrorSpy.mock.calls[0][0] as string;
      const log2Json = consoleErrorSpy.mock.calls[1][0] as string;
      const entry1: LogEntry = JSON.parse(log1Json);
      const entry2: LogEntry = JSON.parse(log2Json);

      expect(entry1.context.errorId).toBeDefined();
      expect(entry2.context.errorId).toBeDefined();
      expect(entry1.context.errorId).not.toBe(entry2.context.errorId);
      expect(entry1.context.errorId).toMatch(/^ERR-[a-z0-9]+-[a-z0-9]+$/);
    });

    it("handles non-Error objects", () => {
      const logger = Logger.forComponent("test");
      logger.error("Failed", "string error");

      const loggedJson = consoleErrorSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);
      const context = entry.context as LogContext & { errorMessage?: string };

      expect(context.errorMessage).toBe("string error");
      expect(context.errorStack).toBeUndefined();
    });

    it("includes additional context with error", () => {
      const logger = Logger.forComponent("test");
      const error = new Error("Test");
      logger.error("Failed", error, { terminalId: "terminal-3", workspacePath: "/test" });

      const loggedJson = consoleErrorSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry.context.terminalId).toBe("terminal-3");
      expect(entry.context.workspacePath).toBe("/test");
      expect(entry.context.errorId).toBeDefined();
    });
  });

  describe("JSON format", () => {
    it("outputs valid JSON for all log levels", () => {
      const logger = Logger.forComponent("test");

      logger.info("info");
      logger.warn("warn");
      logger.error("error", new Error("test"));

      const infoJson = consoleLogSpy.mock.calls[0][0] as string;
      const warnJson = consoleWarnSpy.mock.calls[0][0] as string;
      const errorJson = consoleErrorSpy.mock.calls[0][0] as string;

      expect(() => JSON.parse(infoJson)).not.toThrow();
      expect(() => JSON.parse(warnJson)).not.toThrow();
      expect(() => JSON.parse(errorJson)).not.toThrow();
    });

    it("includes all required fields in log entry", () => {
      const logger = Logger.forComponent("test");
      logger.info("test message", { terminalId: "term-1" });

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry).toHaveProperty("timestamp");
      expect(entry).toHaveProperty("level");
      expect(entry).toHaveProperty("message");
      expect(entry).toHaveProperty("context");
      expect(entry.context).toHaveProperty("component");
    });
  });

  describe("multiple loggers", () => {
    it("maintains separate component names", () => {
      const logger1 = Logger.forComponent("component-1");
      const logger2 = Logger.forComponent("component-2");

      logger1.info("from logger 1");
      logger2.info("from logger 2");

      const log1Json = consoleLogSpy.mock.calls[0][0] as string;
      const log2Json = consoleLogSpy.mock.calls[1][0] as string;
      const entry1: LogEntry = JSON.parse(log1Json);
      const entry2: LogEntry = JSON.parse(log2Json);

      expect(entry1.context.component).toBe("component-1");
      expect(entry2.context.component).toBe("component-2");
    });
  });

  describe("custom context fields", () => {
    it("allows arbitrary context fields", () => {
      const logger = Logger.forComponent("test");
      logger.info("test", {
        customField: "custom value",
        numericField: 42,
        booleanField: true,
      });

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const entry: LogEntry = JSON.parse(loggedJson);

      expect(entry.context.customField).toBe("custom value");
      expect(entry.context.numericField).toBe(42);
      expect(entry.context.booleanField).toBe(true);
    });
  });
});
