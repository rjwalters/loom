import { fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { mockAuthFetch } from "@/test/mock-auth-fetch";
import { renderWithProviders } from "@/test/utils";
import { Layout } from "../Layout";

describe("Layout", () => {
  beforeEach(() => {
    localStorage.clear();
    mockAuthFetch();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("renders header with app name", () => {
    renderWithProviders(
      <Layout>
        <div>Content</div>
      </Layout>,
    );

    expect(screen.getByText("Loom Quickstart")).toBeInTheDocument();
  });

  it("renders navigation links", () => {
    renderWithProviders(
      <Layout>
        <div>Content</div>
      </Layout>,
    );

    expect(screen.getByRole("link", { name: /home/i })).toBeInTheDocument();
  });

  it("renders children content", () => {
    renderWithProviders(
      <Layout>
        <div data-testid="test-content">Test Content</div>
      </Layout>,
    );

    expect(screen.getByTestId("test-content")).toBeInTheDocument();
    expect(screen.getByText("Test Content")).toBeInTheDocument();
  });

  it("shows login button when user is not authenticated", async () => {
    renderWithProviders(
      <Layout>
        <div>Content</div>
      </Layout>,
    );

    await waitFor(() => {
      expect(screen.getByRole("link", { name: /login/i })).toBeInTheDocument();
    });
  });

  it("shows dashboard link when user is authenticated", async () => {
    const user = { id: "1", email: "test@example.com", name: "Test" };
    mockAuthFetch({ user });

    renderWithProviders(
      <Layout>
        <div>Content</div>
      </Layout>,
    );

    await waitFor(() => {
      expect(screen.getByRole("link", { name: /dashboard/i })).toBeInTheDocument();
    });
  });

  it("shows user name and logout button when authenticated", async () => {
    const user = { id: "1", email: "test@example.com", name: "TestUser" };
    mockAuthFetch({ user });

    renderWithProviders(
      <Layout>
        <div>Content</div>
      </Layout>,
    );

    await waitFor(() => {
      expect(screen.getByText("TestUser")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: /logout/i })).toBeInTheDocument();
    });
  });

  it("includes theme toggle in header", () => {
    renderWithProviders(
      <Layout>
        <div>Content</div>
      </Layout>,
    );

    expect(screen.getByRole("button", { name: /toggle theme/i })).toBeInTheDocument();
  });

  it("calls logout when logout button is clicked", async () => {
    const user = { id: "1", email: "test@example.com", name: "TestUser" };
    mockAuthFetch({ user });

    renderWithProviders(
      <Layout>
        <div>Content</div>
      </Layout>,
    );

    await waitFor(() => {
      expect(screen.getByRole("button", { name: /logout/i })).toBeInTheDocument();
    });

    const logoutButton = screen.getByRole("button", { name: /logout/i });
    fireEvent.click(logoutButton);

    await waitFor(() => {
      expect(screen.getByRole("link", { name: /login/i })).toBeInTheDocument();
    });
  });
});
