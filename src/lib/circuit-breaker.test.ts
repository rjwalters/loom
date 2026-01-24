import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  CircuitBreaker,
  CircuitOpenError,
  CircuitState,
  getDaemonCircuitBreaker,
  resetDaemonCircuitBreaker,
} from "./circuit-breaker";

describe("CircuitBreaker", () => {
  let breaker: CircuitBreaker;

  beforeEach(() => {
    vi.useFakeTimers();
    breaker = new CircuitBreaker({
      name: "test-circuit",
      failureThreshold: 3,
      recoveryTimeout: 10000,
      successThreshold: 2,
    });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  describe("initial state", () => {
    it("should start in closed state", () => {
      expect(breaker.getState()).toBe(CircuitState.CLOSED);
    });

    it("should allow operations when closed", () => {
      expect(breaker.canAttempt()).toBe(true);
    });

    it("should be available when closed", () => {
      expect(breaker.isAvailable()).toBe(true);
    });
  });

  describe("execute", () => {
    it("should pass through successful operations", async () => {
      const result = await breaker.execute(async () => "success");
      expect(result).toBe("success");
      expect(breaker.getState()).toBe(CircuitState.CLOSED);
    });

    it("should pass through errors from operations", async () => {
      const error = new Error("operation failed");
      await expect(
        breaker.execute(async () => {
          throw error;
        })
      ).rejects.toThrow(error);
    });

    it("should track failures through execute", async () => {
      for (let i = 0; i < 2; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }
      expect(breaker.getSnapshot().failures).toBe(2);
      expect(breaker.getState()).toBe(CircuitState.CLOSED);
    });
  });

  describe("state transitions: CLOSED -> OPEN", () => {
    it("should open after reaching failure threshold", async () => {
      for (let i = 0; i < 3; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }
      expect(breaker.getState()).toBe(CircuitState.OPEN);
    });

    it("should reset failure count on success", async () => {
      // Two failures
      for (let i = 0; i < 2; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }
      expect(breaker.getSnapshot().failures).toBe(2);

      // One success resets
      await breaker.execute(async () => "ok");
      expect(breaker.getSnapshot().failures).toBe(0);

      // Two more failures shouldn't open (we reset)
      for (let i = 0; i < 2; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }
      expect(breaker.getState()).toBe(CircuitState.CLOSED);
    });

    it("should reject operations when open", async () => {
      // Open the circuit
      for (let i = 0; i < 3; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }

      // Should reject with CircuitOpenError
      await expect(breaker.execute(async () => "ok")).rejects.toThrow(CircuitOpenError);
    });
  });

  describe("state transitions: OPEN -> HALF_OPEN", () => {
    beforeEach(async () => {
      // Open the circuit
      for (let i = 0; i < 3; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }
    });

    it("should transition to half-open after recovery timeout", () => {
      expect(breaker.getState()).toBe(CircuitState.OPEN);
      expect(breaker.canAttempt()).toBe(false);

      // Advance time past recovery timeout
      vi.advanceTimersByTime(10001);

      expect(breaker.canAttempt()).toBe(true);
      expect(breaker.getState()).toBe(CircuitState.HALF_OPEN);
    });

    it("should not transition before recovery timeout", () => {
      vi.advanceTimersByTime(5000);
      expect(breaker.canAttempt()).toBe(false);
      expect(breaker.getState()).toBe(CircuitState.OPEN);
    });
  });

  describe("state transitions: HALF_OPEN -> CLOSED", () => {
    beforeEach(async () => {
      // Open the circuit
      for (let i = 0; i < 3; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }
      // Advance to half-open
      vi.advanceTimersByTime(10001);
      breaker.canAttempt(); // Trigger transition
    });

    it("should close after success threshold in half-open", async () => {
      expect(breaker.getState()).toBe(CircuitState.HALF_OPEN);

      await breaker.execute(async () => "ok");
      expect(breaker.getState()).toBe(CircuitState.HALF_OPEN);

      await breaker.execute(async () => "ok");
      expect(breaker.getState()).toBe(CircuitState.CLOSED);
    });
  });

  describe("state transitions: HALF_OPEN -> OPEN", () => {
    beforeEach(async () => {
      // Open the circuit
      for (let i = 0; i < 3; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }
      // Advance to half-open
      vi.advanceTimersByTime(10001);
      breaker.canAttempt(); // Trigger transition
    });

    it("should re-open on any failure in half-open", async () => {
      expect(breaker.getState()).toBe(CircuitState.HALF_OPEN);

      await expect(
        breaker.execute(async () => {
          throw new Error();
        })
      ).rejects.toThrow();
      expect(breaker.getState()).toBe(CircuitState.OPEN);
    });
  });

  describe("events", () => {
    it("should emit state-change events", async () => {
      const events: unknown[] = [];
      breaker.onEvent((event) => events.push(event));

      // Open the circuit
      for (let i = 0; i < 3; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }

      expect(events).toContainEqual({
        type: "state-change",
        from: CircuitState.CLOSED,
        to: CircuitState.OPEN,
      });
    });

    it("should emit failure events", async () => {
      const events: unknown[] = [];
      breaker.onEvent((event) => events.push(event));

      await expect(
        breaker.execute(async () => {
          throw new Error();
        })
      ).rejects.toThrow();

      expect(events).toContainEqual({
        type: "failure",
        failures: 1,
        threshold: 3,
      });
    });

    it("should emit success events", async () => {
      const events: unknown[] = [];
      breaker.onEvent((event) => events.push(event));

      await breaker.execute(async () => "ok");

      expect(events).toContainEqual({
        type: "success",
        state: CircuitState.CLOSED,
      });
    });

    it("should emit rejected events when open", async () => {
      // Open the circuit
      for (let i = 0; i < 3; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }

      const events: unknown[] = [];
      breaker.onEvent((event) => events.push(event));

      await expect(breaker.execute(async () => "ok")).rejects.toThrow(CircuitOpenError);

      expect(events).toContainEqual({
        type: "rejected",
        state: CircuitState.OPEN,
      });
    });

    it("should allow unsubscribing from events", async () => {
      const events: unknown[] = [];
      const unsubscribe = breaker.onEvent((event) => events.push(event));

      await breaker.execute(async () => "ok");
      expect(events.length).toBeGreaterThan(0);

      const countBefore = events.length;
      unsubscribe();

      await breaker.execute(async () => "ok");
      expect(events.length).toBe(countBefore);
    });
  });

  describe("reset", () => {
    it("should reset to closed state", async () => {
      // Open the circuit
      for (let i = 0; i < 3; i++) {
        await expect(
          breaker.execute(async () => {
            throw new Error();
          })
        ).rejects.toThrow();
      }
      expect(breaker.getState()).toBe(CircuitState.OPEN);

      breaker.reset();

      expect(breaker.getState()).toBe(CircuitState.CLOSED);
      expect(breaker.getSnapshot().failures).toBe(0);
      expect(breaker.getSnapshot().successes).toBe(0);
    });
  });

  describe("forceState", () => {
    it("should force state transition", () => {
      breaker.forceState(CircuitState.OPEN);
      expect(breaker.getState()).toBe(CircuitState.OPEN);

      breaker.forceState(CircuitState.HALF_OPEN);
      expect(breaker.getState()).toBe(CircuitState.HALF_OPEN);

      breaker.forceState(CircuitState.CLOSED);
      expect(breaker.getState()).toBe(CircuitState.CLOSED);
    });
  });

  describe("getSnapshot", () => {
    it("should return current state snapshot", async () => {
      await breaker.execute(async () => "ok");
      await expect(
        breaker.execute(async () => {
          throw new Error();
        })
      ).rejects.toThrow();

      const snapshot = breaker.getSnapshot();

      expect(snapshot.state).toBe(CircuitState.CLOSED);
      expect(snapshot.failures).toBe(1);
      expect(snapshot.successes).toBe(1);
      expect(snapshot.lastSuccessTime).not.toBeNull();
      expect(snapshot.lastFailureTime).not.toBeNull();
    });
  });
});

describe("getDaemonCircuitBreaker", () => {
  beforeEach(() => {
    resetDaemonCircuitBreaker();
  });

  it("should return singleton instance", () => {
    const breaker1 = getDaemonCircuitBreaker();
    const breaker2 = getDaemonCircuitBreaker();
    expect(breaker1).toBe(breaker2);
  });

  it("should have correct default configuration", () => {
    const breaker = getDaemonCircuitBreaker();
    expect(breaker.getName()).toBe("daemon-ipc");
  });
});

describe("CircuitOpenError", () => {
  it("should have correct properties", () => {
    const error = new CircuitOpenError("test", CircuitState.OPEN);

    expect(error.name).toBe("CircuitOpenError");
    expect(error.circuitName).toBe("test");
    expect(error.state).toBe(CircuitState.OPEN);
    expect(error.message).toContain("test");
    expect(error.message).toContain("open");
  });
});
