# Retry Command Action

A custom GitHub Action for retrying commands with configurable attempts, timeouts, and backoff delays.

## Features

- ✅ Configurable retry attempts
- ✅ Per-attempt timeout
- ✅ Configurable wait time between retries
- ✅ Clear logging with visual indicators
- ✅ Supports any shell command
- ✅ Zero external dependencies

## Usage

```yaml
- name: Install dependencies with retry
  uses: ./.github/actions/retry-command
  with:
    command: npm ci --prefer-offline
    max_attempts: 3
    retry_wait_seconds: 5
    timeout_minutes: 5
```

## Inputs

| Input | Description | Required | Default |
|-------|-------------|----------|---------|
| `command` | Command to execute | Yes | - |
| `max_attempts` | Maximum number of attempts | No | `3` |
| `retry_wait_seconds` | Seconds to wait between retries | No | `5` |
| `timeout_minutes` | Timeout in minutes for each attempt | No | `10` |
| `shell` | Shell to use for command execution | No | `bash` |

## Examples

### Basic retry with defaults

```yaml
- uses: ./.github/actions/retry-command
  with:
    command: npm install
```

### Custom configuration

```yaml
- uses: ./.github/actions/retry-command
  with:
    command: cargo build --release
    max_attempts: 5
    retry_wait_seconds: 10
    timeout_minutes: 15
```

### Different shell

```yaml
- uses: ./.github/actions/retry-command
  with:
    command: Get-Process
    shell: pwsh
```

## Output Format

The action provides clear visual feedback:

```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🔄 Retry Command Configuration
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Command: npm ci --prefer-offline
Max attempts: 3
Retry wait: 5s
Timeout per attempt: 5m
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

📍 Attempt 1 of 3
⏰ Started at: 2025-01-01 12:00:00 UTC
...command output...

✅ Command succeeded on attempt 1
```

## Why Custom Action?

We use a custom action instead of third-party alternatives for:

1. **Control**: Full control over retry logic and updates
2. **Security**: No external dependencies or supply chain risks
3. **Simplicity**: Pure bash implementation, easy to understand
4. **Maintenance**: Lives in our repository, versioned with our code
5. **Reliability**: No risk of third-party actions being deprecated or removed

## License

Same as the main Loom project.
