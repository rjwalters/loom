# Loom Quickstart: Webapp

A modern web application template pre-configured for Loom AI-powered development.

## Stack

- **Frontend**: React 19, TypeScript, Tailwind CSS 4, shadcn/ui
- **Backend**: Cloudflare Pages Functions
- **Database**: Cloudflare D1 (SQLite at the edge)
- **Build**: Vite
- **Linting**: Biome

## Features

- User authentication (login/logout/register)
- Dark/light theme with system preference detection
- D1 database with migrations
- Responsive dashboard layout
- Pre-configured Loom roles and workflows

## Quick Start

### 1. Copy the template

```bash
# From the Loom repository
cp -r quickstarts/webapp ~/projects/my-app
cd ~/projects/my-app

# Initialize git
git init
git add -A
git commit -m "Initial commit from loom-quickstart-webapp"
```

### 2. Install dependencies

```bash
pnpm install
```

### 3. Set up Cloudflare D1

```bash
# Login to Cloudflare
npx wrangler login

# Create D1 database
npx wrangler d1 create loom-quickstart-db

# Update wrangler.toml with the database_id from output

# Run migrations
pnpm db:migrate
```

### 4. Start development

```bash
pnpm dev
```

Visit `http://localhost:5173` to see your app.

## Project Structure

```
├── functions/          # Cloudflare Pages Functions (API)
│   └── api/
│       └── [[route]].ts  # Catch-all API handler
├── migrations/         # D1 database migrations
├── public/             # Static assets
├── src/
│   ├── components/     # React components
│   │   └── ui/         # shadcn/ui components
│   ├── hooks/          # Custom React hooks
│   ├── lib/            # Utility functions
│   ├── pages/          # Page components
│   └── styles/         # Global styles
├── .loom/              # Loom configuration
│   ├── roles/          # Role definitions
│   └── scripts/        # Helper scripts
└── .github/            # GitHub configuration
    └── labels.yml      # Loom workflow labels
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
   # Make your changes...
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

### Adding new pages

1. Create component in `src/pages/MyPage.tsx`
2. Add route in `src/App.tsx`
3. Optionally add to navigation in `src/components/Layout.tsx`

### Adding API endpoints

Edit `functions/api/[[route]].ts` to add new routes:

```typescript
if (path === "/api/my-endpoint" && method === "GET") {
  return json({ data: "Hello" });
}
```

### Database changes

1. Create new migration in `migrations/0002_my_change.sql`
2. Apply locally: `pnpm db:migrate`
3. Apply to production: `pnpm db:migrate:prod`

### Theming

CSS variables are defined in `src/styles/globals.css`. Modify the `:root` and `.dark` selectors to customize colors.

## Deployment

### Cloudflare Pages

```bash
# Build the app
pnpm build

# Deploy to Cloudflare Pages
pnpm deploy
```

Or connect your repository to Cloudflare Pages for automatic deployments on push.

### Environment Variables

Set in Cloudflare dashboard or via `wrangler secret`:

```bash
npx wrangler secret put MY_SECRET
```

## Scripts

| Script | Description |
|--------|-------------|
| `pnpm dev` | Start development server |
| `pnpm build` | Build for production |
| `pnpm preview` | Preview production build |
| `pnpm deploy` | Deploy to Cloudflare Pages |
| `pnpm db:migrate` | Run D1 migrations locally |
| `pnpm db:migrate:prod` | Run D1 migrations in production |
| `pnpm lint` | Check code with Biome |
| `pnpm lint:fix` | Fix linting issues |
| `pnpm test` | Run tests |

## Learn More

- [Loom Documentation](https://github.com/loomhq/loom)
- [Cloudflare Pages](https://developers.cloudflare.com/pages/)
- [Cloudflare D1](https://developers.cloudflare.com/d1/)
- [React](https://react.dev)
- [Tailwind CSS](https://tailwindcss.com)
- [shadcn/ui](https://ui.shadcn.com)
