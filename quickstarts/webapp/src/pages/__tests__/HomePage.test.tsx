import { screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { renderWithProviders } from "@/test/utils";
import { HomePage } from "../HomePage";

describe("HomePage", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("renders main heading", () => {
    renderWithProviders(<HomePage />);

    expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("Loom Quickstart Webapp");
  });

  it("renders description text", () => {
    renderWithProviders(<HomePage />);

    expect(screen.getByText(/modern web application template/i)).toBeInTheDocument();
  });

  it("shows Get Started and Learn More buttons when not authenticated", () => {
    renderWithProviders(<HomePage />);

    expect(screen.getByRole("link", { name: /get started/i })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /learn more/i })).toBeInTheDocument();
  });

  it("shows Go to Dashboard button when authenticated", async () => {
    const user = { id: "1", email: "test@example.com", name: "Test" };
    localStorage.setItem("loom-quickstart-auth", JSON.stringify(user));

    renderWithProviders(<HomePage />);

    await waitFor(() => {
      expect(screen.getByRole("link", { name: /go to dashboard/i })).toBeInTheDocument();
    });
  });

  it("renders feature cards", () => {
    renderWithProviders(<HomePage />);

    expect(screen.getByText("Authentication")).toBeInTheDocument();
    expect(screen.getByText("Dark Mode")).toBeInTheDocument();
    expect(screen.getByText("D1 Database")).toBeInTheDocument();
    expect(screen.getByText("Loom Ready")).toBeInTheDocument();
    expect(screen.getByText("Modern Stack")).toBeInTheDocument();
    expect(screen.getByText("Edge Deployment")).toBeInTheDocument();
  });

  it("links Get Started to login page", () => {
    renderWithProviders(<HomePage />);

    const getStartedLink = screen.getByRole("link", { name: /get started/i });
    expect(getStartedLink).toHaveAttribute("href", "/login");
  });

  it("Learn More links to external loom repository", () => {
    renderWithProviders(<HomePage />);

    const learnMoreLink = screen.getByRole("link", { name: /learn more/i });
    expect(learnMoreLink).toHaveAttribute("href", "https://github.com/loomhq/loom");
    expect(learnMoreLink).toHaveAttribute("target", "_blank");
    expect(learnMoreLink).toHaveAttribute("rel", "noopener noreferrer");
  });
});
