/**
 * Test utilities for asserting against structured JSON logs
 */

import { expect } from "vitest";

export interface ParsedLog {
  timestamp: string;
  level: string;
  message: string;
  context?: Record<string, unknown>;
}

/**
 * Parse a JSON log string into a structured object
 */
export function parseLog(logString: string): ParsedLog {
  try {
    return JSON.parse(logString);
  } catch (error) {
    throw new Error(`Failed to parse log string as JSON: ${logString}\nError: ${error}`);
  }
}

/**
 * Assert that a console spy call contains a JSON log with the expected message
 */
export function expectLogMessage(
  spy: { mock: { calls: unknown[][] } },
  callIndex: number,
  expectedMessage: string,
  level: "INFO" | "WARN" | "ERROR" = "INFO"
) {
  const call = spy.mock.calls[callIndex];
  expect(call, `Expected call ${callIndex} to exist`).toBeDefined();

  const logString = call[0] as string;
  const log = parseLog(logString);

  expect(log.level).toBe(level);
  expect(log.message).toBe(expectedMessage);
}

/**
 * Assert that a console spy call contains a JSON log with a message containing the expected substring
 */
export function expectLogContains(
  spy: { mock: { calls: unknown[][] } },
  callIndex: number,
  expectedSubstring: string,
  level: "INFO" | "WARN" | "ERROR" = "INFO"
) {
  const call = spy.mock.calls[callIndex];
  expect(call, `Expected call ${callIndex} to exist`).toBeDefined();

  const logString = call[0] as string;
  const log = parseLog(logString);

  expect(log.level).toBe(level);
  expect(log.message).toContain(expectedSubstring);
}

/**
 * Assert that a console spy was called with a log matching the expected message
 */
export function expectLogCalled(
  spy: { mock: { calls: unknown[][] } },
  expectedMessage: string,
  level: "INFO" | "WARN" | "ERROR" = "INFO"
) {
  const calls = spy.mock.calls;
  const matchingCall = calls.find((call) => {
    const logString = call[0] as string;
    try {
      const log = parseLog(logString);
      return log.level === level && log.message === expectedMessage;
    } catch {
      return false;
    }
  });

  expect(
    matchingCall,
    `Expected to find log with message "${expectedMessage}" and level "${level}"`
  ).toBeDefined();
}

/**
 * Assert that a console spy was called with a log containing the expected substring
 */
export function expectLogCalledContaining(
  spy: { mock: { calls: unknown[][] } },
  expectedSubstring: string,
  level: "INFO" | "WARN" | "ERROR" = "INFO"
) {
  const calls = spy.mock.calls;
  const matchingCall = calls.find((call) => {
    const logString = call[0] as string;
    try {
      const log = parseLog(logString);
      return log.level === level && log.message.includes(expectedSubstring);
    } catch {
      return false;
    }
  });

  expect(
    matchingCall,
    `Expected to find log containing "${expectedSubstring}" with level "${level}"`
  ).toBeDefined();
}
