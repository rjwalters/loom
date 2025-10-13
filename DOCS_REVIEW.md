# Documentation Review & Recommendations

This document reviews the current state of Loom's documentation and provides recommendations for improvements.

## Current Documentation Structure

### Core Documentation
1. **README.md** - Project overview, vision, architecture, roadmap
2. **DEVELOPMENT.md** - Development setup, code quality tools, workflow
3. **DEV_WORKFLOW.md** - Hot reload workflow with persistent daemon (NEW)
4. **WORKFLOWS.md** - Label-based workflow coordination
5. **CLAUDE.md** - Comprehensive AI development context

### Specialized Documentation
6. **scripts/README.md** - Daemon management scripts (NEW)
7. **defaults/roles/README.md** - Role definition guide
8. **loom-daemon/tests/README.md** - Daemon integration tests

## Documentation Quality Assessment

### ✅ Strengths

1. **Comprehensive Coverage**: Documentation covers vision, architecture, development, and workflows
2. **Clear Quick Starts**: Both README and DEV_WORKFLOW have clear getting-started sections
3. **Role Documentation**: Well-documented role system with examples
4. **Code Quality**: DEVELOPMENT.md clearly explains linting, formatting, and CI
5. **New Dev Workflow**: DEV_WORKFLOW.md provides excellent guidance for hot reload development

### ⚠️ Areas for Improvement

#### 1. Documentation Overlap & Inconsistency

**Problem**: Multiple docs describe setup/development with slight variations

**Files with overlap:**
- README.md (lines 169-187): Basic setup instructions
- DEVELOPMENT.md (lines 13-28): Similar setup instructions
- DEV_WORKFLOW.md (lines 5-56): New simplified approach

**Inconsistencies:**
- README.md shows old two-terminal approach:
  ```bash
  # Terminal 1
  cd loom-daemon && cargo run

  # Terminal 2
  pnpm tauri dev
  ```
- DEV_WORKFLOW.md shows new one-command approach:
  ```bash
  pnpm run app:dev
  ```

**Recommendation:**
- **README.md** should reference DEV_WORKFLOW.md for detailed dev setup
- Keep only the simplest quick start in README
- Make DEV_WORKFLOW.md the canonical development guide

#### 2. Missing Cross-References

**Problem**: Related docs don't link to each other

**Examples:**
- DEVELOPMENT.md doesn't mention DEV_WORKFLOW.md
- README.md "Development Setup" doesn't link to DEV_WORKFLOW.md
- scripts/README.md exists but isn't mentioned in main docs

**Recommendation**: Add cross-reference section to each doc

#### 3. Package.json Script Documentation

**Problem**: New pnpm scripts (app:dev, app:dev:restart, etc.) aren't documented in DEVELOPMENT.md

**Missing commands:**
```bash
pnpm run app:dev
pnpm run app:dev:restart
pnpm run app:stop
pnpm run daemon:test:scripts
```

**Recommendation**: Update DEVELOPMENT.md "Available Commands" section

#### 4. Testing Documentation Gaps

**Problem**:
- README.md mentions daemon tests but not script tests
- No documentation of test-daemon-scripts.sh
- DEVELOPMENT.md doesn't cover testing

**Recommendation**: Add testing section to DEVELOPMENT.md covering:
- Unit tests (cargo test)
- Integration tests (daemon:test)
- Script tests (daemon:test:scripts)

#### 5. DEV_WORKFLOW.md Length

**Observation**: At 281 lines, DEV_WORKFLOW.md is very comprehensive but might be overwhelming

**Recommendation**: Consider splitting into:
- Quick reference (scenarios, commands)
- Technical deep-dive (how reconnection works, debugging)

## Proposed Documentation Structure

```
README.md
├─> Quick start (one command: pnpm run app:dev)
├─> Vision & architecture overview
├─> Link to: "See DEV_WORKFLOW.md for detailed development guide"
└─> Link to: "See WORKFLOWS.md for agent coordination"

DEV_WORKFLOW.md (Primary development guide)
├─> Quick start (pnpm run app:dev)
├─> Development scenarios
├─> Troubleshooting
├─> Available commands (complete reference)
└─> Technical details (how it works)

DEVELOPMENT.md (Code quality & best practices)
├─> Prerequisites & setup
├─> Code quality tools
├─> Testing
├─> Git workflow
└─> Link to: "See DEV_WORKFLOW.md for day-to-day development"

CLAUDE.md (AI development context)
├─> Project context for AI assistants
├─> Architecture patterns
└─> Should reference DEV_WORKFLOW.md for current practices

scripts/README.md
├─> Script reference
└─> Should be linked from DEV_WORKFLOW.md
```

## Recommended Updates

### 1. Update README.md Development Setup

**Current (lines 169-187):**
```markdown
## ⚙️ Development Setup

```bash
# Clone the repository
git clone https://github.com/rjwalters/loom.git
cd loom

# Install dependencies
pnpm install

# Configure environment
cp .env.example .env
# Edit with your API keys and workspace path

# Start daemon (Terminal 1)
cd loom-daemon
cargo run

# Start GUI (Terminal 2)
pnpm tauri dev
```
```

**Recommended:**
```markdown
## ⚙️ Development Setup

```bash
# Clone the repository
git clone https://github.com/rjwalters/loom.git
cd loom

# Install dependencies
pnpm install

# Start development environment (daemon + GUI)
pnpm run app:dev
```

**For detailed development workflows, troubleshooting, and advanced usage, see [DEV_WORKFLOW.md](DEV_WORKFLOW.md).**
```

### 2. Update DEVELOPMENT.md with New Commands

Add to "Available Commands" section (after line 86):

```markdown
#### Application Development Commands
```bash
# Start daemon + Tauri dev in one command
pnpm run app:dev

# Restart daemon when it gets into bad state
pnpm run app:dev:restart

# Stop the background daemon
pnpm run app:stop
```

#### Daemon Management
```bash
# Start daemon in background
pnpm run daemon:start

# Stop daemon
pnpm run daemon:stop

# Restart daemon
pnpm run daemon:restart

# Run daemon in foreground (for debugging)
pnpm run daemon:dev
```

For detailed workflow information, see [DEV_WORKFLOW.md](DEV_WORKFLOW.md).
```

### 3. Add Testing Section to DEVELOPMENT.md

Add after "Git Hooks" section (after line 97):

```markdown
### Testing

Loom has comprehensive testing at multiple levels:

#### Unit Tests
```bash
# Run all workspace tests
cargo test --workspace

# Run with verbose output
cargo test --workspace -- --nocapture
```

#### Integration Tests (Daemon)
```bash
# Run daemon integration tests
pnpm run daemon:test

# Run with verbose output
pnpm run daemon:test:verbose

# Run specific test
cargo test --test integration_basic test_ping_pong -- --nocapture
```

#### Script Integration Tests
```bash
# Test daemon management scripts
pnpm run daemon:test:scripts
```

**Requirements**: Tests require `tmux` installed (`brew install tmux` on macOS)

See [scripts/README.md](scripts/README.md) for details on daemon management script testing.
```

### 4. Add Cross-Reference Section to Each Doc

**README.md** (add at end of Development Setup):
```markdown
### Additional Development Resources
- [DEV_WORKFLOW.md](DEV_WORKFLOW.md) - Detailed development workflow with hot reload
- [DEVELOPMENT.md](DEVELOPMENT.md) - Code quality, testing, and best practices
- [WORKFLOWS.md](WORKFLOWS.md) - Agent coordination via GitHub labels
- [scripts/README.md](scripts/README.md) - Daemon management scripts
```

**DEVELOPMENT.md** (add at beginning after Prerequisites):
```markdown
## Documentation Overview

This guide covers code quality, tooling, and development practices. For:
- **Day-to-day development workflow**: See [DEV_WORKFLOW.md](DEV_WORKFLOW.md)
- **Project vision and architecture**: See [README.md](README.md)
- **Agent workflows**: See [WORKFLOWS.md](WORKFLOWS.md)
```

**DEV_WORKFLOW.md** (add at beginning after first paragraph):
```markdown
## Related Documentation

- [README.md](README.md) - Project overview and quick start
- [DEVELOPMENT.md](DEVELOPMENT.md) - Code quality and testing
- [scripts/README.md](scripts/README.md) - Daemon script reference
- [CLAUDE.md](CLAUDE.md) - AI development context
```

### 5. Update CLAUDE.md Development Workflow

Update the package manager preference section (around line 30):

```markdown
**Package Manager Preference**: Always use `pnpm` (not `npm`) as the package manager for this project.

**Development Workflow**: Use `pnpm run app:dev` to start the daemon and Tauri dev server in one command. See [DEV_WORKFLOW.md](DEV_WORKFLOW.md) for details on hot reload workflow with persistent daemon connections.
```

## Summary of Changes Needed

| File | Action | Priority |
|------|--------|----------|
| README.md | Simplify dev setup, add cross-references | High |
| DEVELOPMENT.md | Add new commands, testing section, cross-references | High |
| DEV_WORKFLOW.md | Add cross-references at top | Medium |
| CLAUDE.md | Reference DEV_WORKFLOW.md | Medium |
| .github/workflows/ci.yml | Consider adding script tests | Low |

## Future Considerations

### 1. Documentation for End Users

Once Loom is released, consider adding:
- **USER_GUIDE.md** - How to use Loom (not develop it)
- **TROUBLESHOOTING.md** - Common issues and solutions
- **CONFIGURATION.md** - Workspace configuration reference

### 2. Video/Visual Documentation

Consider adding:
- Architecture diagrams (update README.md diagram)
- Screenshot of the UI showing terminal roles
- GIF showing hot reload workflow
- Screencast of daemon reconnection

### 3. API Documentation

Future additions:
- **API.md** - Daemon IPC protocol
- **PLUGIN.md** - Creating custom roles
- JSDoc/rustdoc integration

## Metrics

Current documentation state:

| Metric | Count |
|--------|-------|
| Total markdown files | 23 |
| Core documentation files | 5 |
| Lines of documentation | ~2,500+ |
| Cross-references | 8 (needs ~20) |
| Code examples | 50+ |
| Diagrams | 2 |

## Conclusion

Loom has strong foundational documentation, particularly:
- ✅ Excellent project vision (README.md)
- ✅ Comprehensive development context (CLAUDE.md)
- ✅ Detailed hot reload workflow (DEV_WORKFLOW.md)
- ✅ Clear role system documentation

Key improvements needed:
1. **Consistency**: Update all docs to reference new `pnpm run app:dev` workflow
2. **Cross-references**: Link related docs to each other
3. **Command documentation**: Document new daemon management commands
4. **Testing docs**: Expand testing coverage in DEVELOPMENT.md

Priority: Implement recommendations 1-4 above to bring documentation to production-ready quality.
