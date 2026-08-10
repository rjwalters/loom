import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mockAuthFetch } from "../../test/mock-auth-fetch";
import { AuthProvider, useAuth } from "../use-auth";

function wrapper({ children }: { children: React.ReactNode }) {
  return <AuthProvider>{children}</AuthProvider>;
}

describe("useAuth", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("throws when used outside AuthProvider", () => {
    expect(() => {
      renderHook(() => useAuth());
    }).toThrow("useAuth must be used within an AuthProvider");
  });

  it("starts with no user after initial load", async () => {
    mockAuthFetch();

    const { result } = renderHook(() => useAuth(), { wrapper });

    await waitFor(() => {
      expect(result.current.isLoading).toBe(false);
    });

    expect(result.current.user).toBeNull();
  });

  it("restores existing session from /api/auth/me on mount", async () => {
    const storedUser = {
      id: "stored-id",
      email: "stored@example.com",
      name: "Stored User",
    };
    mockAuthFetch({ user: storedUser });

    const { result } = renderHook(() => useAuth(), { wrapper });

    await waitFor(() => {
      expect(result.current.isLoading).toBe(false);
    });

    expect(result.current.user).toEqual(storedUser);
  });

  it("throws error for empty email", async () => {
    mockAuthFetch();

    const { result, unmount } = renderHook(() => useAuth(), { wrapper });

    await waitFor(() => {
      expect(result.current.isLoading).toBe(false);
    });

    let error: Error | null = null;
    try {
      await act(async () => {
        await result.current.login("", "password");
      });
    } catch (e) {
      error = e as Error;
    }

    expect(error).not.toBeNull();
    expect(error?.message).toBe("Email and password are required");

    unmount();
  });

  it("registers new user successfully", async () => {
    mockAuthFetch();

    const { result } = renderHook(() => useAuth(), { wrapper });

    await waitFor(() => {
      expect(result.current.isLoading).toBe(false);
    });

    await act(async () => {
      await result.current.register("new@example.com", "password123", "New User");
    });

    expect(result.current.user).not.toBeNull();
    expect(result.current.user?.email).toBe("new@example.com");
    expect(result.current.user?.name).toBe("New User");
  });

  it("sends credentials with the login request", async () => {
    const fetchSpy = mockAuthFetch();

    const { result } = renderHook(() => useAuth(), { wrapper });

    await waitFor(() => {
      expect(result.current.isLoading).toBe(false);
    });

    await act(async () => {
      await result.current.login("test@example.com", "password");
    });

    const loginCall = fetchSpy.mock.calls.find(([input]) => String(input).endsWith("/api/auth/login"));
    expect(loginCall).toBeDefined();
    expect(loginCall?.[1]).toMatchObject({ credentials: "include" });
  });

  // Test login last to isolate any cleanup issues
  it("logs in user successfully", async () => {
    mockAuthFetch();

    const { result } = renderHook(() => useAuth(), { wrapper });

    await waitFor(() => {
      expect(result.current.isLoading).toBe(false);
    });

    await act(async () => {
      await result.current.login("test@example.com", "password123");
    });

    expect(result.current.user).not.toBeNull();
    expect(result.current.user?.email).toBe("test@example.com");
    expect(result.current.user?.name).toBe("test");
  });
});
