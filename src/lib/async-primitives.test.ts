import { describe, expect, it, vi } from "vitest";
import { AsyncLock, PromiseDeduplicator } from "./async-primitives";

describe("AsyncLock", () => {
  it("allows single acquisition", async () => {
    const lock = new AsyncLock();

    expect(lock.isLocked()).toBe(false);

    const release = await lock.acquire();

    expect(lock.isLocked()).toBe(true);

    release();

    expect(lock.isLocked()).toBe(false);
  });

  it("prevents concurrent access", async () => {
    const lock = new AsyncLock();
    const execOrder: number[] = [];

    // First task acquires lock, holds for 50ms
    const task1 = (async () => {
      const release = await lock.acquire();
      execOrder.push(1);
      await new Promise((resolve) => setTimeout(resolve, 50));
      execOrder.push(2);
      release();
    })();

    // Give task1 a moment to acquire the lock
    await new Promise((resolve) => setTimeout(resolve, 10));

    // Second task waits for lock
    const task2 = (async () => {
      const release = await lock.acquire();
      execOrder.push(3);
      release();
    })();

    await Promise.all([task1, task2]);

    // Task 1 should complete before task 2 starts
    expect(execOrder).toEqual([1, 2, 3]);
  });

  it("processes waiting queue in order", async () => {
    const lock = new AsyncLock();
    const execOrder: number[] = [];

    // Acquire lock first
    const release1 = await lock.acquire();

    // Queue up multiple waiters
    const task2 = lock.acquire().then((release) => {
      execOrder.push(2);
      release();
    });

    const task3 = lock.acquire().then((release) => {
      execOrder.push(3);
      release();
    });

    const task4 = lock.acquire().then((release) => {
      execOrder.push(4);
      release();
    });

    expect(lock.queueLength).toBe(3);

    // Release the initial lock
    release1();

    await Promise.all([task2, task3, task4]);

    // Should process in FIFO order
    expect(execOrder).toEqual([2, 3, 4]);
  });

  it("reports queue length correctly", async () => {
    const lock = new AsyncLock();

    expect(lock.queueLength).toBe(0);

    const release = await lock.acquire();

    // Queue up waiters
    const promises = [lock.acquire(), lock.acquire(), lock.acquire()];

    // Give time for promises to queue
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(lock.queueLength).toBe(3);

    release();

    await Promise.all(promises.map((p) => p.then((r) => r())));

    expect(lock.queueLength).toBe(0);
  });
});

describe("PromiseDeduplicator", () => {
  it("executes operation normally when not in flight", async () => {
    const dedup = new PromiseDeduplicator<string>();

    const result = await dedup.execute("key1", async () => {
      return "success";
    });

    expect(result).toBe("success");
  });

  it("skips duplicate in-flight operations", async () => {
    const dedup = new PromiseDeduplicator<string>();
    let execCount = 0;

    const operation = async () => {
      execCount++;
      await new Promise((resolve) => setTimeout(resolve, 50));
      return "result";
    };

    // Start both at the same time
    const [result1, result2] = await Promise.all([
      dedup.execute("key1", operation),
      dedup.execute("key1", operation),
    ]);

    expect(result1).toBe("result");
    expect(result2).toBeUndefined();
    expect(execCount).toBe(1);
  });

  it("allows different keys to execute concurrently", async () => {
    const dedup = new PromiseDeduplicator<string>();
    let execCount = 0;

    const operation = async (key: string) => {
      execCount++;
      await new Promise((resolve) => setTimeout(resolve, 50));
      return `result-${key}`;
    };

    const [result1, result2] = await Promise.all([
      dedup.execute("key1", () => operation("key1")),
      dedup.execute("key2", () => operation("key2")),
    ]);

    expect(result1).toBe("result-key1");
    expect(result2).toBe("result-key2");
    expect(execCount).toBe(2);
  });

  it("reports in-flight status correctly", async () => {
    const dedup = new PromiseDeduplicator<void>();

    expect(dedup.isInFlight("key1")).toBe(false);

    let resolveOp: () => void;
    const opPromise = new Promise<void>((resolve) => {
      resolveOp = resolve;
    });

    const execPromise = dedup.execute("key1", () => opPromise);

    // Give time for execution to start
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(dedup.isInFlight("key1")).toBe(true);

    resolveOp!();
    await execPromise;

    expect(dedup.isInFlight("key1")).toBe(false);
  });

  it("clears in-flight status on error", async () => {
    const dedup = new PromiseDeduplicator<string>();

    const failingOperation = async () => {
      throw new Error("test error");
    };

    await expect(dedup.execute("key1", failingOperation)).rejects.toThrow("test error");

    // Should clear in-flight status even after error
    expect(dedup.isInFlight("key1")).toBe(false);
  });

  it("allows re-execution after previous completes", async () => {
    const dedup = new PromiseDeduplicator<number>();
    let counter = 0;

    const operation = async () => {
      counter++;
      return counter;
    };

    const result1 = await dedup.execute("key1", operation);
    expect(result1).toBe(1);

    const result2 = await dedup.execute("key1", operation);
    expect(result2).toBe(2);
  });

  it("returns in-flight keys", async () => {
    const dedup = new PromiseDeduplicator<void>();

    let resolveOp1: () => void;
    let resolveOp2: () => void;

    const op1 = new Promise<void>((resolve) => {
      resolveOp1 = resolve;
    });
    const op2 = new Promise<void>((resolve) => {
      resolveOp2 = resolve;
    });

    const exec1 = dedup.execute("key1", () => op1);
    const exec2 = dedup.execute("key2", () => op2);

    // Give time for executions to start
    await new Promise((resolve) => setTimeout(resolve, 10));

    const keys = dedup.getInFlightKeys();
    expect(keys).toContain("key1");
    expect(keys).toContain("key2");
    expect(dedup.inFlightCount).toBe(2);

    resolveOp1!();
    resolveOp2!();
    await Promise.all([exec1, exec2]);

    expect(dedup.getInFlightKeys()).toEqual([]);
    expect(dedup.inFlightCount).toBe(0);
  });

  it("handles void operations correctly", async () => {
    const dedup = new PromiseDeduplicator<void>();
    const sideEffect = vi.fn();

    await dedup.execute("key1", async () => {
      sideEffect();
    });

    expect(sideEffect).toHaveBeenCalledTimes(1);
  });
});
