import { fireEvent, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { renderWithProviders } from "@/test/utils";
import { ThemeToggle } from "../ThemeToggle";

describe("ThemeToggle", () => {
  it("renders toggle button", () => {
    renderWithProviders(<ThemeToggle />);

    const button = screen.getByRole("button", { name: /toggle theme/i });
    expect(button).toBeInTheDocument();
  });

  it("toggles from light to dark on click", () => {
    renderWithProviders(<ThemeToggle />);

    const button = screen.getByRole("button", { name: /toggle theme/i });
    fireEvent.click(button);

    expect(document.documentElement.classList.contains("dark")).toBe(true);
  });

  it("toggles back from dark to light", () => {
    renderWithProviders(<ThemeToggle />);

    const button = screen.getByRole("button", { name: /toggle theme/i });

    // Click to go dark
    fireEvent.click(button);
    expect(document.documentElement.classList.contains("dark")).toBe(true);

    // Click to go light
    fireEvent.click(button);
    expect(document.documentElement.classList.contains("light")).toBe(true);
  });

  it("has accessible aria-label", () => {
    renderWithProviders(<ThemeToggle />);

    const button = screen.getByLabelText("Toggle theme");
    expect(button).toBeInTheDocument();
  });
});
