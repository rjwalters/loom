import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Logger } from "./logger";

describe("Logger", () => {
  let consoleLogSpy: ReturnType<typeof vi.spyOn>;
  let consoleWarnSpy: ReturnType<typeof vi.spyOn>;
  let consoleErrorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    // Mock console methods
    consoleLogSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    consoleWarnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    consoleErrorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  describe("Logger Creation", () => {
    it("should create logger for component", () => {
      const logger = Logger.forComponent("test-component");
      expect(logger).toBeInstanceOf(Logger);
    });
  });

  describe("Structured Log Formatting", () => {
    it("should format INFO log entry with timestamp and component", () => {
      const logger = Logger.forComponent("test-component");
      logger.info("Test message");

      expect(consoleLogSpy).toHaveBeenCalledTimes(1);
      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry).toHaveProperty("timestamp");
      expect(logEntry.timestamp).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
      expect(logEntry.level).toBe("INFO");
      expect(logEntry.message).toBe("Test message");
      expect(logEntry.context.component).toBe("test-component");
    });

    it("should format WARN log entry", () => {
      const logger = Logger.forComponent("test-component");
      logger.warn("Warning message");

      expect(consoleWarnSpy).toHaveBeenCalledTimes(1);
      const loggedJson = consoleWarnSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.level).toBe("WARN");
      expect(logEntry.message).toBe("Warning message");
    });

    it("should format ERROR log entry with error details", () => {
      const logger = Logger.forComponent("test-component");
      const testError = new Error("Test error");
      logger.error("Error occurred", testError);

      expect(consoleErrorSpy).toHaveBeenCalledTimes(1);
      const loggedJson = consoleErrorSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.level).toBe("ERROR");
      expect(logEntry.message).toBe("Error occurred");
      expect(logEntry.context.errorMessage).toBe("Test error");
      expect(logEntry.context.errorStack).toBeDefined();
      expect(logEntry.context.errorId).toMatch(/^ERR-/);
    });
  });

  describe("Context Merging", () => {
    it("should merge custom context with component", () => {
      const logger = Logger.forComponent("test-component");
      logger.info("Test message", {
        terminalId: "terminal-1",
        workspacePath: "/path/to/workspace",
      });

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.context.component).toBe("test-component");
      expect(logEntry.context.terminalId).toBe("terminal-1");
      expect(logEntry.context.workspacePath).toBe("/path/to/workspace");
    });

    it("should handle empty context", () => {
      const logger = Logger.forComponent("test-component");
      logger.info("Test message");

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.context).toEqual({ component: "test-component" });
    });

    it("should handle additional custom fields in context", () => {
      const logger = Logger.forComponent("test-component");
      logger.info("Test message", {
        customField1: "value1",
        customField2: 42,
        customField3: true,
      });

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.context.customField1).toBe("value1");
      expect(logEntry.context.customField2).toBe(42);
      expect(logEntry.context.customField3).toBe(true);
    });
  });

  describe("Error ID Generation", () => {
    it("should generate unique error IDs", () => {
      const logger = Logger.forComponent("test-component");
      const error1 = new Error("Error 1");
      const error2 = new Error("Error 2");

      logger.error("First error", error1);
      logger.error("Second error", error2);

      const errorId1 = JSON.parse(consoleErrorSpy.mock.calls[0][0] as string).context.errorId;
      const errorId2 = JSON.parse(consoleErrorSpy.mock.calls[1][0] as string).context.errorId;

      expect(errorId1).toMatch(/^ERR-/);
      expect(errorId2).toMatch(/^ERR-/);
      expect(errorId1).not.toBe(errorId2);
    });

    it("should include errorId in context for error logs", () => {
      const logger = Logger.forComponent("test-component");
      const error = new Error("Test error");
      logger.error("Error message", error, { terminalId: "terminal-1" });

      const loggedJson = consoleErrorSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.context.errorId).toBeDefined();
      expect(logEntry.context.errorId).toMatch(/^ERR-/);
      expect(logEntry.context.terminalId).toBe("terminal-1");
    });
  });

  describe("Error Handling", () => {
    it("should handle Error objects", () => {
      const logger = Logger.forComponent("test-component");
      const error = new Error("Test error");
      error.stack = "Test stack trace";

      logger.error("Error occurred", error);

      const loggedJson = consoleErrorSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.context.errorMessage).toBe("Test error");
      expect(logEntry.context.errorStack).toBe("Test stack trace");
    });

    it("should handle non-Error objects", () => {
      const logger = Logger.forComponent("test-component");
      logger.error("Error occurred", "String error");

      const loggedJson = consoleErrorSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.context.errorMessage).toBe("String error");
      expect(logEntry.context.errorStack).toBeUndefined();
    });

    it("should handle null/undefined errors", () => {
      const logger = Logger.forComponent("test-component");
      logger.error("Error occurred", null);

      const loggedJson = consoleErrorSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.context.errorMessage).toBe("null");
    });
  });

  describe("Console Method Selection", () => {
    it("should use console.log for INFO level", () => {
      const logger = Logger.forComponent("test-component");
      logger.info("Info message");

      expect(consoleLogSpy).toHaveBeenCalledTimes(1);
      expect(consoleWarnSpy).not.toHaveBeenCalled();
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });

    it("should use console.warn for WARN level", () => {
      const logger = Logger.forComponent("test-component");
      logger.warn("Warn message");

      expect(consoleWarnSpy).toHaveBeenCalledTimes(1);
      expect(consoleLogSpy).not.toHaveBeenCalled();
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });

    it("should use console.error for ERROR level", () => {
      const logger = Logger.forComponent("test-component");
      logger.error("Error message", new Error("Test"));

      expect(consoleErrorSpy).toHaveBeenCalledTimes(1);
      expect(consoleLogSpy).not.toHaveBeenCalled();
      expect(consoleWarnSpy).not.toHaveBeenCalled();
    });
  });

  describe("JSON Output Format", () => {
    it("should output valid JSON", () => {
      const logger = Logger.forComponent("test-component");
      logger.info("Test message");

      const output = consoleLogSpy.mock.calls[0][0] as string;
      expect(() => JSON.parse(output)).not.toThrow();
    });

    it("should escape special characters in message", () => {
      const logger = Logger.forComponent("test-component");
      logger.info('Message with "quotes" and \n newlines');

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.message).toBe('Message with "quotes" and \n newlines');
    });

    it("should handle complex nested context objects", () => {
      const logger = Logger.forComponent("test-component");
      logger.info("Test message", {
        nested: {
          field1: "value1",
          field2: {
            deepField: "deepValue",
          },
        },
      });

      const loggedJson = consoleLogSpy.mock.calls[0][0] as string;
      const logEntry = JSON.parse(loggedJson);

      expect(logEntry.context.nested.field1).toBe("value1");
      expect(logEntry.context.nested.field2.deepField).toBe("deepValue");
    });
  });

  describe("Multiple Logger Instances", () => {
    it("should maintain separate component names for different loggers", () => {
      const logger1 = Logger.forComponent("component-1");
      const logger2 = Logger.forComponent("component-2");

      logger1.info("Message from component 1");
      logger2.info("Message from component 2");

      const log1 = JSON.parse(consoleLogSpy.mock.calls[0][0] as string);
      const log2 = JSON.parse(consoleLogSpy.mock.calls[1][0] as string);

      expect(log1.context.component).toBe("component-1");
      expect(log2.context.component).toBe("component-2");
    });
  });
});
