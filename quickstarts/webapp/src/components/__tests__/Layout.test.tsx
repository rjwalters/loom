import { fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { renderWithProviders } from "@/test/utils";
import { Layout } from "../Layout";

describe("Layout", () => {
  beforeEach(() => {
    localStorage.clear();
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
    // Pre-set authenticated user in localStorage
    const user = { id: "1", email: "test@example.com", name: "Test" };
    localStorage.setItem("loom-quickstart-auth", JSON.stringify(user));

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
    localStorage.setItem("loom-quickstart-auth", JSON.stringify(user));

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
    localStorage.setItem("loom-quickstart-auth", JSON.stringify(user));

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
