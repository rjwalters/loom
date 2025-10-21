# Testing Guide

Loom's testing strategy combines multiple approaches to ensure comprehensive coverage while maintaining fast feedback loops.

## Testing Stack

### Frontend Unit Tests

- **Framework**: Vitest 3.2.4 with happy-dom environment
- **Coverage**: V8 provider with 80% thresholds
- **Mocking**: `@tauri-apps/api/mocks` for Tauri IPC calls
- **Setup**: Global test setup with WebCrypto polyfill

### Backend Integration Tests

- **Framework**: Rust with `tokio::test`
- **Coverage**: Daemon IPC protocol, terminal lifecycle, security validation
- **Isolation**: Serial execution with temp directories and unique socket paths

## Project Structure

```
loom/
├── src/
│   ├── lib/*.test.ts       # Frontend unit tests
│   └── test/
│       └── setup.ts        # Global test configuration
├── loom-daemon/
│   └── tests/
│       ├── common/         # Test utilities
│       ├── integration_basic.rs
│       └── integration_security.rs
├── vitest.config.ts        # Vitest configuration
└── docs/testing/           # Testing documentation
```

## Running Tests

### Frontend Unit Tests

```bash
# Run all frontend tests
pnpm test:unit

# Run with coverage report
pnpm test:unit:coverage

# Run tests in watch mode (development)
pnpm exec vitest

# Run specific test file
pnpm test:unit src/lib/state.test.ts

# Run tests matching pattern
pnpm test:unit -t "terminal CRUD"
```

### Backend Integration Tests

```bash
# Run all daemon tests
pnpm daemon:test

# Run with verbose output
pnpm daemon:test:verbose

# Run specific test file
cargo test --test integration_basic

# Run specific test
cargo test --test integration_basic test_ping_pong
```

### Full CI Suite

```bash
# Run complete CI pipeline locally
pnpm check:ci

# This runs:
# - Biome linting and formatting
# - Rust formatting (rustfmt)
# - Clippy with CI flags
# - Frontend build
# - All tests
```

## Writing Frontend Tests

### Basic Test Structure

```typescript
import { describe, it, expect, beforeEach } from "vitest";
import { MyComponent } from "./my-component";

describe("MyComponent", () => {
	beforeEach(() => {
		// Setup runs before each test
		document.body.innerHTML = '<div id="container"></div>';
	});

	it("should render correctly", () => {
		MyComponent.render();
		const container = document.getElementById("container");
		expect(container?.innerHTML).toContain("Expected content");
	});
});
```

### Using Tauri Mocks

Tauri mocks are configured globally in `src/test/setup.ts`:

```typescript
// Default mocks are already configured!
// Just import and use Tauri APIs in tests:

import { invoke } from "@tauri-apps/api/tauri";

it("should validate git repository", async () => {
	// The mock is already set up to return true
	const isValid = await invoke<boolean>("validate_git_repo", {
		path: "/path/to/repo",
	});

	expect(isValid).toBe(true);
});
```

### Customizing Mocks Per Test

Override default mocks for specific tests:

```typescript
import { beforeEach } from "vitest";
import { mockIPC } from "@tauri-apps/api/mocks";

beforeEach(() => {
	// Custom mock for this test suite
	mockIPC((cmd, payload) => {
		if (cmd === "read_text_file") {
			return Promise.resolve('{"customData": "value"}');
		}
		// Fall through to default mocks
		return null;
	});
});
```

### Testing State Management

```typescript
import { describe, it, expect, beforeEach, vi } from "vitest";
import { AppState } from "./state";

describe("State Management", () => {
	let state: AppState;

	beforeEach(() => {
		state = new AppState();
	});

	it("should notify listeners on state change", () => {
		const callback = vi.fn();
		state.onChange(callback);

		state.addTerminal({
			id: "test-1",
			name: "Test Terminal",
			status: TerminalStatus.Idle,
			isPrimary: true,
		});

		expect(callback).toHaveBeenCalledTimes(1);
	});
});
```

### Testing UI Rendering

```typescript
import { describe, it, expect, beforeEach } from "vitest";
import { renderHeader } from "./ui";

describe("UI Rendering", () => {
	beforeEach(() => {
		// Set up DOM structure
		document.body.innerHTML = '<div id="workspace-name"></div>';
	});

	it("should render workspace name", () => {
		renderHeader("/path/to/loom", true);

		const container = document.getElementById("workspace-name");
		expect(container?.innerHTML).toContain("loom");
	});

	it("should escape HTML for XSS protection", () => {
		renderHeader("/path/to/<script>alert('xss')</script>", true);

		const container = document.getElementById("workspace-name");
		expect(container?.innerHTML).not.toContain("<script>");
	});
});
```

## Test Coverage

### Current Coverage Thresholds

Configured in `vitest.config.ts`:

```typescript
coverage: {
  thresholds: {
    lines: 80,
    functions: 80,
    branches: 80,
    statements: 80,
  },
}
```

### Viewing Coverage Reports

```bash
# Generate coverage report
pnpm test:unit:coverage

# Open HTML report in browser
open coverage/index.html
```

### What to Test

**Do test:**

- ✅ State mutations and observer pattern
- ✅ UI rendering with various inputs
- ✅ Input validation and sanitization
- ✅ Error handling paths
- ✅ IPC call patterns (mocked)

**Don't test:**

- ❌ Third-party library internals
- ❌ Browser APIs directly
- ❌ Actual network requests
- ❌ Real file system operations

## Testing Best Practices

### 1. Test Behavior, Not Implementation

**❌ Bad:**

```typescript
it("should call setState internally", () => {
	const spy = vi.spyOn(component, "setState");
	component.updateValue("test");
	expect(spy).toHaveBeenCalled();
});
```

**✅ Good:**

```typescript
it("should update displayed value", () => {
	component.updateValue("test");
	expect(component.getValue()).toBe("test");
});
```

### 2. Use Descriptive Test Names

**❌ Bad:**

```typescript
it("works", () => {
	/* ... */
});
```

**✅ Good:**

```typescript
it("should notify listeners when terminal is added", () => {
	/* ... */
});
```

### 3. Arrange-Act-Assert Pattern

```typescript
it("should update terminal status", () => {
	// Arrange
	const terminal = createTestTerminal();
	state.addTerminal(terminal);

	// Act
	state.updateTerminal(terminal.id, { status: TerminalStatus.Busy });

	// Assert
	const updated = state.getTerminal(terminal.id);
	expect(updated.status).toBe(TerminalStatus.Busy);
});
```

### 4. One Assertion Per Test (When Possible)

**❌ Avoid:**

```typescript
it("should handle terminal lifecycle", () => {
	state.addTerminal(terminal); // Tests multiple things
	expect(state.getTerminals()).toHaveLength(1);
	state.removeTerminal(terminal.id);
	expect(state.getTerminals()).toHaveLength(0);
	state.setPrimary(terminal.id);
	// ... too much in one test
});
```

**✅ Prefer:**

```typescript
it("should add terminal", () => {
	state.addTerminal(terminal);
	expect(state.getTerminals()).toHaveLength(1);
});

it("should remove terminal", () => {
	state.addTerminal(terminal);
	state.removeTerminal(terminal.id);
	expect(state.getTerminals()).toHaveLength(0);
});
```

### 5. Clean Up After Tests

```typescript
afterEach(() => {
	// Reset DOM
	document.body.innerHTML = "";

	// Clear vi mocks
	vi.clearAllMocks();

	// Reset module state if needed
	vi.resetModules();
});
```

## Debugging Tests

### VS Code Configuration

Add to `.vscode/launch.json`:

```json
{
	"type": "node",
	"request": "launch",
	"name": "Debug Vitest Tests",
	"runtimeExecutable": "pnpm",
	"runtimeArgs": ["exec", "vitest", "--run", "--no-coverage"],
	"console": "integratedTerminal"
}
```

### Debugging Specific Tests

```bash
# Run single test with debugger
pnpm exec vitest --run --no-coverage -t "specific test name"
```

### Common Issues

**Issue**: Tests pass locally but fail in CI

**Solution**: Run full CI suite locally before pushing

```bash
pnpm check:ci
```

**Issue**: Flaky tests due to timing

**Solution**: Avoid time-dependent assertions, use deterministic mocks

```typescript
// ❌ Bad - time-dependent
it("should debounce after 500ms", async () => {
	await sleep(500);
	expect(callback).toHaveBeenCalled();
});

// ✅ Good - deterministic
it("should debounce", () => {
	vi.useFakeTimers();
	callDebounced();
	vi.advanceTimersByTime(500);
	expect(callback).toHaveBeenCalled();
	vi.useRealTimers();
});
```

## Hybrid Testing Strategy

Loom uses a hybrid approach:

### Unit Tests (Fast, Mocked)

- **What**: State management, UI rendering, utility functions
- **How**: Vitest with Tauri mocks
- **When**: Every commit (via pre-commit hook)
- **Speed**: ~20 seconds for 500+ tests

### Integration Tests (Moderate, Real IPC)

- **What**: Daemon protocol, terminal lifecycle, security
- **How**: Rust `tokio::test` with real tmux
- **When**: Pre-push and CI
- **Speed**: ~30 seconds for daemon suite

### Manual E2E (Slow, Manual)

- **What**: Full workflows (workspace start, agent launch, factory reset)
- **How**: Manual testing checklist
- **When**: Before release
- **Speed**: 10-15 minutes

### Why No Automated E2E?

Tauri's E2E tool (`tauri-driver`) doesn't support macOS (our primary platform). We use comprehensive unit tests + daemon integration tests + manual E2E checklist instead. See [ADR-001: E2E Testing Strategy](../adr/001-e2e-testing-strategy.md) for full rationale.

## WebCrypto Polyfill

Tauri APIs use WebCrypto, but happy-dom doesn't provide it. Our test setup adds a polyfill:

```typescript
// src/test/setup.ts
import { webcrypto } from "node:crypto";

beforeAll(() => {
	Object.defineProperty(global, "crypto", {
		value: webcrypto,
		writable: true,
		configurable: true,
	});
});
```

This allows Tauri mocks to work correctly in tests.

## References

- [Vitest Documentation](https://vitest.dev/)
- [Tauri Testing Guide](https://v2.tauri.app/develop/tests/)
- [Testing Library Principles](https://testing-library.com/docs/guiding-principles/)
- [Loom ADR-001: E2E Testing Strategy](../adr/001-e2e-testing-strategy.md)

## Contributing

When adding new features:

1. **Write tests first** (TDD when possible)
2. **Maintain coverage** above 80%
3. **Run full CI suite** before creating PR: `pnpm check:ci`
4. **Document edge cases** in test names and comments

See [CONTRIBUTING.md](../../CONTRIBUTING.md) for full guidelines.
