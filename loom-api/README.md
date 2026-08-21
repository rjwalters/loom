# loom-api

External REST API server for Loom analytics data. Provides read-only HTTP
endpoints over a Loom workspace's metrics for integration with tools like
Grafana, custom dashboards, and CI/CD pipelines.

## Usage

```bash
# Start the API server (default port 9999)
loom-api --workspace /path/to/workspace

# Custom port
loom-api --workspace /path/to/workspace --port 8080
```

## Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/v1/health` | Health check |
| `GET /api/v1/metrics/summary` | Overall agent metrics |
| `GET /api/v1/metrics/velocity` | Velocity summary with trends |
| `GET /api/v1/metrics/roles` | Metrics broken down by role |
| `GET /api/v1/patterns` | Prompt patterns catalog |
| `GET /api/v1/recommendations` | Active recommendations |

See the crate-level docs in [`src/main.rs`](src/main.rs) for details.
