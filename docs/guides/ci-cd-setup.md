# Loom CI/CD Integration Guide

This guide shows how to integrate Loom workspace initialization into continuous integration and deployment pipelines.

## Table of Contents

- [Overview](#overview)
- [GitHub Actions](#github-actions)
- [GitLab CI](#gitlab-ci)
- [Jenkins](#jenkins)
- [CircleCI](#circleci)
- [Docker Integration](#docker-integration)
- [Bulk Repository Setup](#bulk-repository-setup)
- [Environment-Specific Defaults](#environment-specific-defaults)
- [Best Practices](#best-practices)
- [Troubleshooting](#troubleshooting)

## Overview

Loom's `loom-daemon init` command enables headless workspace initialization, making it perfect for CI/CD pipelines where you want to:

- **Initialize Loom** in every repository automatically
- **Sync default configurations** across multiple projects
- **Enforce organizational standards** via custom defaults
- **Prepare repositories** for AI agent orchestration
- **Validate** Loom configuration in CI checks

**Key Benefits:**
- ✅ No GUI required
- ✅ Idempotent (safe to run multiple times)
- ✅ Fast execution (< 1 second typical)
- ✅ Clear exit codes for error handling
- ✅ Supports custom organizational defaults

## GitHub Actions

### Basic Setup

Create `.github/workflows/loom-setup.yml`:

```yaml
name: Initialize Loom Workspace

on:
  push:
    branches: [main]
  pull_request:

jobs:
  setup-loom:
    runs-on: macos-latest  # Loom currently requires macOS

    steps:
      - name: Checkout repository
        uses: actions/checkout@v4

      - name: Download Loom daemon
        run: |
          # Download from GitHub releases
          curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
            -o loom-daemon
          chmod +x loom-daemon

      - name: Initialize Loom workspace
        run: ./loom-daemon init --force

      - name: Verify initialization
        run: |
          test -d .loom || exit 1
          test -f .loom/config.json || exit 1
          test -f CLAUDE.md || exit 1
          echo "✓ Loom workspace initialized successfully"
```

### With Custom Organizational Defaults

```yaml
name: Initialize Loom with Org Defaults

on:
  push:
    branches: [main]

jobs:
  setup-loom:
    runs-on: macos-latest

    steps:
      - name: Checkout repository
        uses: actions/checkout@v4

      - name: Checkout org defaults
        uses: actions/checkout@v4
        with:
          repository: your-org/loom-defaults
          path: loom-defaults
          token: ${{ secrets.ORG_PAT }}

      - name: Download Loom daemon
        run: |
          curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
            -o loom-daemon
          chmod +x loom-daemon

      - name: Initialize with org defaults
        run: ./loom-daemon init --force --defaults ./loom-defaults

      - name: Commit changes (if any)
        run: |
          git config user.name "Loom Bot"
          git config user.email "loom-bot@your-org.com"
          git add .loom CLAUDE.md AGENTS.md .claude .gitignore
          git diff --staged --quiet || git commit -m "chore: update Loom configuration"
          git push
```

### Validate Loom Configuration

```yaml
name: Validate Loom Setup

on:
  pull_request:

jobs:
  validate-loom:
    runs-on: macos-latest

    steps:
      - uses: actions/checkout@v4

      - name: Download Loom daemon
        run: |
          curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
            -o loom-daemon
          chmod +x loom-daemon

      - name: Validate Loom configuration
        run: |
          # Check that required files exist
          if [ ! -d .loom ]; then
            echo "❌ .loom directory missing"
            exit 1
          fi

          if [ ! -f .loom/config.json ]; then
            echo "❌ .loom/config.json missing"
            exit 1
          fi

          # Validate config.json is valid JSON
          if ! jq empty .loom/config.json 2>/dev/null; then
            echo "❌ .loom/config.json is invalid JSON"
            exit 1
          fi

          # Check CLAUDE.md exists
          if [ ! -f CLAUDE.md ]; then
            echo "❌ CLAUDE.md missing"
            exit 1
          fi

          echo "✓ Loom configuration valid"

      - name: Check for drift from defaults
        run: |
          # Initialize in temp location to compare
          ./loom-daemon init --force /tmp/loom-check

          # Compare critical files
          diff .loom/config.json /tmp/loom-check/.loom/config.json || \
            echo "⚠️  Warning: config.json differs from defaults"
```

### Reusable Workflow

Create `.github/workflows/loom-init.yml`:

```yaml
name: Loom Initialization (Reusable)

on:
  workflow_call:
    inputs:
      defaults-repo:
        description: 'Repository containing custom defaults'
        required: false
        type: string
      force:
        description: 'Force reinitialization'
        required: false
        type: boolean
        default: false

jobs:
  initialize:
    runs-on: macos-latest

    steps:
      - uses: actions/checkout@v4

      - name: Checkout custom defaults
        if: inputs.defaults-repo != ''
        uses: actions/checkout@v4
        with:
          repository: ${{ inputs.defaults-repo }}
          path: loom-defaults
          token: ${{ secrets.GITHUB_TOKEN }}

      - name: Download Loom daemon
        run: |
          curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
            -o loom-daemon
          chmod +x loom-daemon

      - name: Initialize Loom
        run: |
          FLAGS=""
          if [ "${{ inputs.force }}" = "true" ]; then
            FLAGS="--force"
          fi
          if [ -d loom-defaults ]; then
            FLAGS="$FLAGS --defaults ./loom-defaults"
          fi
          ./loom-daemon init $FLAGS
```

Use in other workflows:

```yaml
name: Main Workflow

on: [push]

jobs:
  setup:
    uses: ./.github/workflows/loom-init.yml
    with:
      defaults-repo: your-org/loom-defaults
      force: true

  build:
    needs: setup
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - run: # your build steps
```

## GitLab CI

### Basic Setup

Create `.gitlab-ci.yml`:

```yaml
stages:
  - setup
  - validate

setup-loom:
  stage: setup
  image: macos-latest  # Requires macOS runner
  script:
    - curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon -o loom-daemon
    - chmod +x loom-daemon
    - ./loom-daemon init --force
  artifacts:
    paths:
      - .loom/
      - CLAUDE.md
      - AGENTS.md
      - .claude/
    expire_in: 1 week

validate-loom:
  stage: validate
  image: macos-latest
  dependencies:
    - setup-loom
  script:
    - test -d .loom || exit 1
    - test -f .loom/config.json || exit 1
    - echo "✓ Loom workspace valid"
```

### With Custom Defaults

```yaml
variables:
  LOOM_DEFAULTS_REPO: https://gitlab.com/your-org/loom-defaults.git

setup-loom:
  stage: setup
  script:
    # Clone defaults repository
    - git clone $LOOM_DEFAULTS_REPO loom-defaults

    # Download daemon
    - curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon -o loom-daemon
    - chmod +x loom-daemon

    # Initialize with custom defaults
    - ./loom-daemon init --force --defaults ./loom-defaults
  artifacts:
    paths:
      - .loom/
      - CLAUDE.md
```

### Scheduled Sync

```yaml
sync-loom-config:
  stage: setup
  only:
    - schedules  # Run on scheduled pipelines
  script:
    - git clone $LOOM_DEFAULTS_REPO loom-defaults
    - curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon -o loom-daemon
    - chmod +x loom-daemon
    - ./loom-daemon init --force --defaults ./loom-defaults

    # Commit if changes detected
    - git config user.name "Loom Sync Bot"
    - git config user.email "loom-bot@your-org.com"
    - git add .loom CLAUDE.md AGENTS.md .claude .gitignore
    - git diff --staged --quiet || (git commit -m "chore: sync Loom config" && git push)
```

## Jenkins

### Declarative Pipeline

Create `Jenkinsfile`:

```groovy
pipeline {
    agent {
        label 'macos'  // Requires macOS agent
    }

    stages {
        stage('Setup Loom') {
            steps {
                script {
                    // Download daemon
                    sh '''
                        curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
                          -o loom-daemon
                        chmod +x loom-daemon
                    '''

                    // Initialize workspace
                    sh './loom-daemon init --force'
                }
            }
        }

        stage('Validate') {
            steps {
                sh '''
                    test -d .loom || exit 1
                    test -f .loom/config.json || exit 1
                    echo "✓ Loom initialized"
                '''
            }
        }
    }
}
```

### With Custom Defaults

```groovy
pipeline {
    agent { label 'macos' }

    environment {
        LOOM_DEFAULTS_REPO = 'https://github.com/your-org/loom-defaults.git'
    }

    stages {
        stage('Checkout Defaults') {
            steps {
                dir('loom-defaults') {
                    git url: env.LOOM_DEFAULTS_REPO, branch: 'main'
                }
            }
        }

        stage('Initialize Loom') {
            steps {
                sh '''
                    curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
                      -o loom-daemon
                    chmod +x loom-daemon
                    ./loom-daemon init --force --defaults ./loom-defaults
                '''
            }
        }
    }
}
```

### Scripted Pipeline

```groovy
node('macos') {
    stage('Setup') {
        checkout scm

        sh '''
            curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
              -o loom-daemon
            chmod +x loom-daemon
        '''
    }

    stage('Initialize Loom') {
        sh './loom-daemon init --force'

        // Verify
        if (!fileExists('.loom/config.json')) {
            error 'Loom initialization failed'
        }
    }
}
```

## CircleCI

### Basic Configuration

Create `.circleci/config.yml`:

```yaml
version: 2.1

executors:
  macos-executor:
    macos:
      xcode: "15.0"  # Requires macOS executor

jobs:
  setup-loom:
    executor: macos-executor
    steps:
      - checkout

      - run:
          name: Download Loom daemon
          command: |
            curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
              -o loom-daemon
            chmod +x loom-daemon

      - run:
          name: Initialize Loom workspace
          command: ./loom-daemon init --force

      - run:
          name: Verify initialization
          command: |
            test -d .loom || exit 1
            test -f .loom/config.json || exit 1
            echo "✓ Loom initialized"

      - persist_to_workspace:
          root: .
          paths:
            - .loom
            - CLAUDE.md
            - AGENTS.md

workflows:
  main:
    jobs:
      - setup-loom
```

### With Custom Defaults

```yaml
version: 2.1

jobs:
  setup-loom:
    macos:
      xcode: "15.0"
    steps:
      - checkout

      - run:
          name: Checkout defaults
          command: |
            git clone https://github.com/your-org/loom-defaults.git loom-defaults

      - run:
          name: Download daemon
          command: |
            curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
              -o loom-daemon
            chmod +x loom-daemon

      - run:
          name: Initialize with org defaults
          command: ./loom-daemon init --force --defaults ./loom-defaults
```

## Docker Integration

### Dockerfile for Loom Initialization

**Note:** Loom currently requires macOS. This example shows the pattern for future Linux support.

```dockerfile
FROM rust:1.75 AS builder

# Build loom-daemon
WORKDIR /build
COPY loom-daemon/ ./
RUN cargo build --release

FROM debian:bookworm-slim

# Install dependencies
RUN apt-get update && apt-get install -y \
    git \
    tmux \
    && rm -rf /var/lib/apt/lists/*

# Copy daemon binary
COPY --from=builder /build/target/release/loom-daemon /usr/local/bin/

# Initialize workspace on container start
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/loom-daemon", "init"]
```

### Docker Compose

```yaml
version: '3.8'

services:
  loom-init:
    image: your-org/loom-daemon:latest
    volumes:
      - ./:/workspace
      - ./custom-defaults:/defaults
    command: ["init", "--force", "--defaults", "/defaults"]
```

### Multi-Repository Setup

```bash
#!/bin/bash
# init-all-repos.sh

REPOS=(
  "/path/to/repo1"
  "/path/to/repo2"
  "/path/to/repo3"
)

for repo in "${REPOS[@]}"; do
  echo "Initializing $repo"
  docker run --rm \
    -v "$repo:/workspace" \
    -v "./org-defaults:/defaults" \
    your-org/loom-daemon:latest \
    init --force --defaults /defaults
done
```

## Bulk Repository Setup

### Shell Script

```bash
#!/bin/bash
# bulk-init.sh - Initialize Loom across multiple repositories

set -e

# Configuration
LOOM_DAEMON="./loom-daemon"
DEFAULTS_PATH="./org-defaults"
REPOS_FILE="repositories.txt"

# Download daemon if not present
if [ ! -f "$LOOM_DAEMON" ]; then
  echo "Downloading loom-daemon..."
  curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
    -o "$LOOM_DAEMON"
  chmod +x "$LOOM_DAEMON"
fi

# Clone defaults repository
if [ ! -d "$DEFAULTS_PATH" ]; then
  echo "Cloning defaults repository..."
  git clone https://github.com/your-org/loom-defaults.git "$DEFAULTS_PATH"
fi

# Read repository paths from file (one per line)
while IFS= read -r repo_path; do
  # Skip empty lines and comments
  [[ -z "$repo_path" || "$repo_path" =~ ^# ]] && continue

  echo "----------------------------------------"
  echo "Initializing: $repo_path"

  # Check if directory exists
  if [ ! -d "$repo_path" ]; then
    echo "⚠️  Skipping (directory not found): $repo_path"
    continue
  fi

  # Initialize
  if "$LOOM_DAEMON" init --force --defaults "$DEFAULTS_PATH" "$repo_path"; then
    echo "✓ Initialized: $repo_path"
  else
    echo "❌ Failed: $repo_path"
  fi
done < "$REPOS_FILE"

echo "----------------------------------------"
echo "Bulk initialization complete"
```

### repositories.txt

```
# List of repositories to initialize
/Users/dev/Projects/repo1
/Users/dev/Projects/repo2
/Users/dev/Projects/repo3

# Can include comments
/Users/dev/Projects/repo4  # Important project
```

### Usage

```bash
# Run bulk initialization
./bulk-init.sh

# Or with logging
./bulk-init.sh 2>&1 | tee init.log
```

### Python Script

```python
#!/usr/bin/env python3
"""Bulk initialize Loom across multiple repositories."""

import subprocess
import sys
from pathlib import Path

# Configuration
LOOM_DAEMON = "./loom-daemon"
DEFAULTS_PATH = "./org-defaults"
REPOS = [
    "/Users/dev/Projects/repo1",
    "/Users/dev/Projects/repo2",
    "/Users/dev/Projects/repo3",
]

def init_repository(repo_path: str) -> bool:
    """Initialize Loom in a repository."""
    print(f"Initializing: {repo_path}")

    repo = Path(repo_path)
    if not repo.exists():
        print(f"⚠️  Skipping (not found): {repo_path}")
        return False

    try:
        result = subprocess.run(
            [LOOM_DAEMON, "init", "--force", "--defaults", DEFAULTS_PATH, str(repo)],
            capture_output=True,
            text=True,
            check=True
        )
        print(f"✓ Initialized: {repo_path}")
        return True
    except subprocess.CalledProcessError as e:
        print(f"❌ Failed: {repo_path}")
        print(f"Error: {e.stderr}")
        return False

def main():
    """Main entry point."""
    print("Bulk Loom initialization")
    print("=" * 40)

    success_count = 0
    fail_count = 0

    for repo in REPOS:
        if init_repository(repo):
            success_count += 1
        else:
            fail_count += 1
        print()

    print("=" * 40)
    print(f"Results: {success_count} succeeded, {fail_count} failed")

    sys.exit(0 if fail_count == 0 else 1)

if __name__ == "__main__":
    main()
```

## Environment-Specific Defaults

### Strategy

Maintain different defaults for different environments:

```
loom-defaults/
├── production/
│   ├── config.json
│   ├── roles/
│   └── CLAUDE.md
├── staging/
│   ├── config.json
│   ├── roles/
│   └── CLAUDE.md
└── development/
    ├── config.json
    ├── roles/
    └── CLAUDE.md
```

### GitHub Actions with Environments

```yaml
name: Initialize Loom by Environment

on:
  push:
    branches: [main, staging, develop]

jobs:
  setup-loom:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4

      - name: Checkout defaults
        uses: actions/checkout@v4
        with:
          repository: your-org/loom-defaults
          path: loom-defaults

      - name: Download daemon
        run: |
          curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon \
            -o loom-daemon
          chmod +x loom-daemon

      - name: Determine environment
        id: env
        run: |
          if [ "${{ github.ref }}" = "refs/heads/main" ]; then
            echo "env=production" >> $GITHUB_OUTPUT
          elif [ "${{ github.ref }}" = "refs/heads/staging" ]; then
            echo "env=staging" >> $GITHUB_OUTPUT
          else
            echo "env=development" >> $GITHUB_OUTPUT
          fi

      - name: Initialize with environment defaults
        run: |
          ./loom-daemon init --force \
            --defaults ./loom-defaults/${{ steps.env.outputs.env }}
```

### GitLab CI with Environments

```yaml
.setup-loom: &setup-loom
  stage: setup
  script:
    - git clone $LOOM_DEFAULTS_REPO loom-defaults
    - curl -L https://github.com/rjwalters/loom/releases/latest/download/loom-daemon -o loom-daemon
    - chmod +x loom-daemon
    - ./loom-daemon init --force --defaults ./loom-defaults/$ENVIRONMENT

setup-production:
  <<: *setup-loom
  only:
    - main
  variables:
    ENVIRONMENT: production

setup-staging:
  <<: *setup-loom
  only:
    - staging
  variables:
    ENVIRONMENT: staging

setup-development:
  <<: *setup-loom
  only:
    - develop
  variables:
    ENVIRONMENT: development
```

## Best Practices

### 1. Version Control Defaults

Store organizational defaults in a separate repository:

```bash
loom-defaults/
├── README.md
├── config.json
├── CLAUDE.md
├── AGENTS.md
├── roles/
├── .claude/
└── .github/
```

Benefits:
- ✅ Centralized configuration management
- ✅ Version history for defaults
- ✅ Team collaboration on standards
- ✅ Easy rollback if needed

### 2. Use `--force` in CI

Always use `--force` flag in CI pipelines:

```bash
loom-daemon init --force
```

Reasons:
- Ensures idempotent behavior
- Prevents failures if .loom exists
- Keeps configuration in sync with defaults

### 3. Validate After Initialization

```bash
# Validate critical files exist
test -f .loom/config.json || exit 1
test -f CLAUDE.md || exit 1

# Validate JSON syntax
jq empty .loom/config.json || exit 1
```

### 4. Cache Dependencies

Cache loom-daemon binary to speed up builds:

```yaml
# GitHub Actions
- name: Cache loom-daemon
  uses: actions/cache@v3
  with:
    path: loom-daemon
    key: loom-daemon-${{ hashFiles('**/loom-version.txt') }}
```

### 5. Fail Fast

Use dry-run before actual initialization:

```bash
# Test if init would succeed
if ! loom-daemon init --dry-run; then
  echo "Dry run failed - aborting"
  exit 1
fi

# Actually initialize
loom-daemon init --force
```

### 6. Log Everything

Enable verbose logging in CI:

```bash
RUST_LOG=info loom-daemon init --force 2>&1 | tee loom-init.log
```

### 7. Version Lock

Pin loom-daemon version in CI:

```bash
LOOM_VERSION="v0.1.0"
curl -L "https://github.com/rjwalters/loom/releases/download/${LOOM_VERSION}/loom-daemon" \
  -o loom-daemon
```

## Troubleshooting

### Issue: "No macOS runner available"

**Problem:** CI platform doesn't have macOS runners

**Solutions:**
1. Use self-hosted macOS runner
2. Wait for Linux support (planned)
3. Initialize locally and commit .loom/

### Issue: Binary download fails

**Problem:** Cannot download loom-daemon binary

**Solutions:**
```bash
# Retry with exponential backoff
for i in 1 2 3; do
  curl -L https://github.com/.../loom-daemon -o loom-daemon && break
  sleep $((i * 2))
done

# Or build from source
git clone https://github.com/rjwalters/loom.git
cd loom
cargo build --release -p loom-daemon
```

### Issue: Permission errors in CI

**Problem:** Cannot write to repository

**Solutions:**
```yaml
# GitHub Actions - use checkout action
- uses: actions/checkout@v4
  with:
    persist-credentials: true

# GitLab CI - check permissions
before_script:
  - chmod -R u+w .
```

### Issue: Defaults repository authentication

**Problem:** Cannot clone private defaults repo

**Solutions:**
```yaml
# GitHub Actions - use PAT
- uses: actions/checkout@v4
  with:
    repository: org/loom-defaults
    token: ${{ secrets.ORG_PAT }}

# GitLab CI - use CI token
git clone https://oauth2:${CI_JOB_TOKEN}@gitlab.com/org/loom-defaults.git
```

## See Also

- [CLI Reference](cli-reference.md) - Complete command documentation
- [Getting Started](getting-started.md) - Installation guide
- [Common Tasks](common-tasks.md) - Development workflows
- [Troubleshooting](troubleshooting.md) - Debugging guide
