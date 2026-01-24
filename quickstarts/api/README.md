# Loom Quickstart: API

A modern API backend template pre-configured for Loom AI-powered development.

## Stack

- **Framework**: Hono (with zod-openapi)
- **Runtime**: Cloudflare Workers
- **Database**: D1 (SQLite) + KV (sessions/cache)
- **Validation**: Zod schemas
- **Documentation**: OpenAPI/Swagger UI
- **Linting**: Biome

## Features

- RESTful route structure with type-safe handlers
- JWT authentication with PBKDF2 password hashing
- Rate limiting via Cloudflare KV
- OpenAPI spec generation with Swagger UI
- CORS and security headers configured
- D1 migrations system
- Pre-configured Loom roles and workflows

## API Endpoints

| Method | Path | Description | Auth |
|--------|------|-------------|------|
| GET | `/health` | Health check | No |
| GET | `/docs` | Swagger UI | No |
| GET | `/openapi.json` | OpenAPI spec | No |
| POST | `/api/auth/register` | Register new user | No |
| POST | `/api/auth/login` | Login, returns JWT | No |
| POST | `/api/auth/refresh` | Refresh JWT | Yes |
| GET | `/api/users` | List users | Admin |
| GET | `/api/users/:id` | Get user | Owner/Admin |
| PUT | `/api/users/:id` | Update user | Owner/Admin |
| DELETE | `/api/users/:id` | Delete user | Admin |

## Quick Start

### 1. Copy the template

```bash
# From the Loom repository
cp -r quickstarts/api ~/projects/my-api
cd ~/projects/my-api

# Initialize git
git init
git add -A
git commit -m "Initial commit from loom-quickstart-api"
```

### 2. Install dependencies

```bash
pnpm install
```

### 3. Set up Cloudflare

```bash
# Login to Cloudflare
npx wrangler login

# Create D1 database
npx wrangler d1 create loom-api-db
# Copy the database_id to wrangler.toml

# Create KV namespace
npx wrangler kv:namespace create "sessions"
# Copy the id to wrangler.toml

# Set JWT secret
npx wrangler secret put JWT_SECRET
# Enter a strong random string

# Run migrations
pnpm db:migrate
```

### 4. Start development

```bash
pnpm dev
```

Visit `http://localhost:8787/docs` to see the Swagger UI.

## Project Structure

```
├── .loom/
│   ├── roles/
│   │   ├── builder.md      # API-specific build guidance
│   │   └── judge.md        # API review criteria
│   └── scripts/
│       └── worktree.sh
├── .github/
│   └── labels.yml
├── migrations/
│   └── 0001_initial.sql    # D1 schema
├── src/
│   ├── routes/
│   │   ├── auth.ts         # Auth endpoints
│   │   ├── users.ts        # User CRUD
│   │   └── health.ts       # Health check
│   ├── middleware/
│   │   ├── auth.ts         # JWT verification, RBAC
│   │   ├── rate-limit.ts   # Rate limiting
│   │   └── error.ts        # Error handling
│   ├── schemas/
│   │   ├── auth.ts         # Auth Zod schemas
│   │   └── user.ts         # User Zod schemas
│   ├── lib/
│   │   ├── jwt.ts          # JWT creation/verification
│   │   └── password.ts     # PBKDF2 hashing
│   ├── types.ts            # Type definitions
│   └── index.ts            # Hono app entry
├── wrangler.toml
├── README.md
└── package.json
```

## Development Workflow with Loom

### Setting up Loom labels

```bash
gh label sync --file .github/labels.yml
```

### Working on an issue

1. Find an issue to work on:
   ```bash
   gh issue list --label="loom:issue"
   ```

2. Claim the issue:
   ```bash
   gh issue edit <number> --remove-label "loom:issue" --add-label "loom:building"
   ```

3. Create a worktree:
   ```bash
   ./.loom/scripts/worktree.sh <number>
   cd .loom/worktrees/issue-<number>
   ```

4. Implement and test:
   ```bash
   pnpm install
   pnpm dev
   # Test with curl or Swagger UI
   pnpm lint
   ```

5. Create a PR:
   ```bash
   git add -A
   git commit -m "Implement feature X"
   git push -u origin feature/issue-<number>
   gh pr create --label "loom:review-requested" --body "Closes #<number>"
   ```

## Customization

### Adding new routes

1. Create schemas in `src/schemas/myschema.ts`:
   ```typescript
   import { z } from "zod";

   export const MyInputSchema = z.object({
     name: z.string().min(1),
   });
   ```

2. Create route file `src/routes/myroute.ts`:
   ```typescript
   import { OpenAPIHono, createRoute } from "@hono/zod-openapi";
   import type { Env } from "../types";
   import { MyInputSchema } from "../schemas/myschema";

   export const myRoutes = new OpenAPIHono<{ Bindings: Env }>();

   const myRoute = createRoute({
     method: "post",
     path: "/",
     tags: ["My Route"],
     request: {
       body: {
         content: { "application/json": { schema: MyInputSchema } },
       },
     },
     responses: {
       200: { description: "Success" },
     },
   });

   myRoutes.openapi(myRoute, async (c) => {
     const { name } = c.req.valid("json");
     return c.json({ message: `Hello, ${name}` });
   });
   ```

3. Mount in `src/index.ts`:
   ```typescript
   import { myRoutes } from "./routes/myroute";
   app.route("/api/my", myRoutes);
   ```

### Adding middleware

```typescript
import type { MiddlewareHandler } from "hono";
import type { Env } from "../types";

export const myMiddleware: MiddlewareHandler<{ Bindings: Env }> = async (c, next) => {
  // Before handler
  console.log("Request:", c.req.url);

  await next();

  // After handler
  console.log("Response:", c.res.status);
};
```

### Database changes

1. Create migration in `migrations/000N_description.sql`
2. Apply locally: `pnpm db:migrate`
3. Apply to production: `pnpm db:migrate:prod`

## Authentication

### Register a user

```bash
curl -X POST http://localhost:8787/api/auth/register \
  -H "Content-Type: application/json" \
  -d '{"email":"user@example.com","password":"password123","name":"Test User"}'
```

### Login

```bash
curl -X POST http://localhost:8787/api/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email":"user@example.com","password":"password123"}'
```

### Use the token

```bash
curl http://localhost:8787/api/users/USER_ID \
  -H "Authorization: Bearer YOUR_TOKEN"
```

## Deployment

```bash
# Deploy to Cloudflare Workers
pnpm deploy

# Run production migrations
pnpm db:migrate:prod
```

## Scripts

| Script | Description |
|--------|-------------|
| `pnpm dev` | Start development server |
| `pnpm deploy` | Deploy to Cloudflare Workers |
| `pnpm db:migrate` | Run D1 migrations locally |
| `pnpm db:migrate:prod` | Run D1 migrations in production |
| `pnpm lint` | Check code with Biome |
| `pnpm lint:fix` | Fix linting issues |
| `pnpm test` | Run tests |
| `pnpm types` | Generate wrangler types |

## Learn More

- [Loom Documentation](https://github.com/loomhq/loom)
- [Hono](https://hono.dev/)
- [Cloudflare Workers](https://developers.cloudflare.com/workers/)
- [Cloudflare D1](https://developers.cloudflare.com/d1/)
- [Zod](https://zod.dev/)
