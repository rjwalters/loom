import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  classifyInputType,
  getInputLogger,
  getLogDate,
  InputLogger,
  resetInputLogger,
} from "./input-logger";

// Mock Tauri invoke
const mockInvoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

// Mock logger
vi.mock("./logger", () => ({
  Logger: {
    forComponent: () => ({
      info: vi.fn(),
      warn: vi.fn(),
      error: vi.fn(),
    }),
  },
}));

describe("InputLogger", () => {
  let inputLogger: InputLogger;

  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
    resetInputLogger();
    inputLogger = new InputLogger();
    mockInvoke.mockResolvedValue(undefined);
  });

  afterEach(() => {
    vi.useRealTimers();
    resetInputLogger();
  });

  describe("start/stop", () => {
    it("starts with workspace path", () => {
      inputLogger.start("/test/workspace");
      expect(inputLogger.isActive()).toBe(true);
    });

    it("stops and clears workspace path", async () => {
      inputLogger.start("/test/workspace");
      await inputLogger.stop();
      expect(inputLogger.isActive()).toBe(false);
    });

    it("flushes buffer on stop", async () => {
      inputLogger.start("/test/workspace");
      inputLogger.log("test", "terminal-1");

      await inputLogger.stop();

      expect(mockInvoke).toHaveBeenCalledWith(
        "append_to_input_log",
        expect.objectContaining({
          workspacePath: "/test/workspace",
        })
      );
    });
  });

  describe("log", () => {
    it("does not log when not started", () => {
      inputLogger.log("test", "terminal-1");
      expect(inputLogger.getBufferSize()).toBe(0);
    });

    it("adds entry to buffer when started", () => {
      inputLogger.start("/test/workspace");
      inputLogger.log("test", "terminal-1");
      expect(inputLogger.getBufferSize()).toBe(1);
    });

    it("schedules flush after logging", () => {
      inputLogger.start("/test/workspace");
      inputLogger.log("test", "terminal-1");

      // Advance timer to trigger flush
      vi.advanceTimersByTime(1000);

      expect(mockInvoke).toHaveBeenCalled();
    });

    it("batches multiple logs before flush", async () => {
      inputLogger.start("/test/workspace");
      inputLogger.log("a", "terminal-1");
      inputLogger.log("b", "terminal-1");
      inputLogger.log("c", "terminal-1");

      expect(inputLogger.getBufferSize()).toBe(3);
      expect(mockInvoke).not.toHaveBeenCalled();

      // Advance timer and run all async operations
      await vi.advanceTimersByTimeAsync(1000);

      // Should have called invoke 3 times (once per entry)
      expect(mockInvoke).toHaveBeenCalledTimes(3);
    });

    it("force flushes when buffer is full", async () => {
      inputLogger.start("/test/workspace");

      // Add 50 entries (max buffer size)
      for (let i = 0; i < 50; i++) {
        inputLogger.log(`entry${i}`, "terminal-1");
      }

      // Should have triggered flush
      await vi.runAllTimersAsync();
      expect(mockInvoke).toHaveBeenCalled();
    });
  });

  describe("flush", () => {
    it("clears buffer after flush", async () => {
      inputLogger.start("/test/workspace");
      inputLogger.log("test", "terminal-1");

      await inputLogger.flush();

      expect(inputLogger.getBufferSize()).toBe(0);
    });

    it("does nothing with empty buffer", async () => {
      inputLogger.start("/test/workspace");
      await inputLogger.flush();
      expect(mockInvoke).not.toHaveBeenCalled();
    });

    it("handles invoke errors gracefully", async () => {
      inputLogger.start("/test/workspace");
      inputLogger.log("test", "terminal-1");
      mockInvoke.mockRejectedValueOnce(new Error("Write failed"));

      // Should not throw
      await expect(inputLogger.flush()).resolves.not.toThrow();
    });
  });
});

describe("classifyInputType", () => {
  it("classifies enter key as 'enter'", () => {
    expect(classifyInputType("\r")).toBe("enter");
    expect(classifyInputType("\n")).toBe("enter");
    expect(classifyInputType("\r\n")).toBe("enter");
  });

  it("classifies command (text + newline) as 'command'", () => {
    expect(classifyInputType("ls\r")).toBe("command");
    expect(classifyInputType("git status\n")).toBe("command");
    expect(classifyInputType("npm run\r\n")).toBe("command");
  });

  it("classifies long input as 'paste'", () => {
    expect(classifyInputType("1234567890")).toBe("paste");
    expect(classifyInputType("this is a long paste")).toBe("paste");
  });

  it("classifies short input as 'keystroke'", () => {
    expect(classifyInputType("a")).toBe("keystroke");
    expect(classifyInputType("ab")).toBe("keystroke");
    expect(classifyInputType("abc")).toBe("keystroke");
  });

  it("classifies edge cases correctly", () => {
    // 9 chars = keystroke (under 10)
    expect(classifyInputType("123456789")).toBe("keystroke");
    // 10 chars = paste
    expect(classifyInputType("1234567890")).toBe("paste");
  });
});

describe("getLogDate", () => {
  it("returns date in YYYY-MM-DD format", () => {
    // Set a fixed date
    vi.setSystemTime(new Date("2026-02-01T12:00:00Z"));

    expect(getLogDate()).toBe("2026-02-01");
  });

  it("pads month and day with zeros", () => {
    vi.setSystemTime(new Date("2026-01-05T12:00:00Z"));

    expect(getLogDate()).toBe("2026-01-05");
  });
});

describe("getInputLogger", () => {
  beforeEach(() => {
    resetInputLogger();
  });

  it("returns singleton instance", () => {
    const logger1 = getInputLogger();
    const logger2 = getInputLogger();
    expect(logger1).toBe(logger2);
  });

  it("creates new instance after reset", () => {
    const logger1 = getInputLogger();
    resetInputLogger();
    const logger2 = getInputLogger();
    expect(logger1).not.toBe(logger2);
  });
});
