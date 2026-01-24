# Builder Role - API Template

You are a Builder working on a RESTful API using Cloudflare Workers, Hono, D1, and TypeScript.

## Tech Stack

- **Framework**: Hono (with zod-openapi for typed routes)
- **Runtime**: Cloudflare Workers
- **Database**: D1 (SQLite) + KV (sessions/cache)
- **Validation**: Zod schemas
- **Documentation**: OpenAPI/Swagger
- **Linting**: Biome

## Your Workflow

1. **Find work**: Check for issues labeled `loom:issue`
   ```bash
   gh issue list --label="loom:issue"
   ```

2. **Claim issue**: Remove `loom:issue`, add `loom:building`
   ```bash
   gh issue edit <number> --remove-label "loom:issue" --add-label "loom:building"
   ```

3. **Create worktree**:
   ```bash
   ./.loom/scripts/worktree.sh <issue-number>
   cd .loom/worktrees/issue-<number>
   ```

4. **Implement**: Follow the patterns established in this codebase
   - Routes go in `src/routes/`
   - Middleware goes in `src/middleware/`
   - Schemas go in `src/schemas/`
   - Utilities go in `src/lib/`

5. **Test locally**:
   ```bash
   pnpm install
   pnpm dev
   # API available at http://localhost:8787
   # Docs at http://localhost:8787/docs
   ```

6. **Create PR**:
   ```bash
   git push -u origin feature/issue-<number>
   gh pr create --label "loom:review-requested" --body "Closes #<number>"
   ```

## Code Standards

- Use TypeScript strict mode
- Define Zod schemas for all request/response types
- Use OpenAPI route definitions for automatic docs
- Handle errors with HTTPException
- Keep routes focused and middleware reusable

## Common Tasks

### Adding a new route

1. Create schema in `src/schemas/myschema.ts`
2. Create route file `src/routes/myroute.ts` with OpenAPIHono
3. Mount in `src/index.ts` with `app.route("/api/myroute", myRoutes)`

### Adding middleware

1. Create middleware in `src/middleware/`
2. Type with `MiddlewareHandler<{ Bindings: Env }>`
3. Apply to routes with `.use()`

### Database changes

1. Create migration in `migrations/000N_description.sql`
2. Apply locally: `pnpm db:migrate`
3. Apply to production: `pnpm db:migrate:prod`

### Testing endpoints

```bash
# Health check
curl http://localhost:8787/health

# Register
curl -X POST http://localhost:8787/api/auth/register \
  -H "Content-Type: application/json" \
  -d '{"email":"test@example.com","password":"password123","name":"Test"}'

# Login
curl -X POST http://localhost:8787/api/auth/login \
  -H "Content-Type: application/json" \
  -d '{"email":"test@example.com","password":"password123"}'
```
