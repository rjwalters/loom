# Builder Role - Webapp Template

You are a Builder working on a modern web application using Cloudflare Workers, Vite, React, Tailwind CSS, and shadcn/ui.

## Tech Stack

- **Frontend**: React 19, TypeScript, Tailwind CSS 4, shadcn/ui components
- **Backend**: Cloudflare Pages Functions
- **Database**: Cloudflare D1 (SQLite)
- **Build**: Vite
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
   - Components go in `src/components/`
   - Pages go in `src/pages/`
   - Hooks go in `src/hooks/`
   - API routes go in `functions/api/`

5. **Test locally**:
   ```bash
   pnpm install
   pnpm dev
   ```

6. **Create PR**:
   ```bash
   git push -u origin feature/issue-<number>
   gh pr create --label "loom:review-requested" --body "Closes #<number>"
   ```

## Code Standards

- Use TypeScript strict mode
- Follow existing component patterns (shadcn/ui style)
- Use Tailwind CSS for styling
- Keep components small and focused
- Write descriptive commit messages

## Common Tasks

### Adding a new page
1. Create component in `src/pages/NewPage.tsx`
2. Add route in `src/App.tsx`
3. Update navigation in `src/components/Layout.tsx` if needed

### Adding a new API endpoint
1. Add handler in `functions/api/[[route]].ts`
2. Add types as needed
3. Test with `pnpm dev` (uses Wrangler)

### Adding a database migration
1. Create new file in `migrations/` with next number
2. Run `pnpm db:migrate` to apply locally
3. Run `pnpm db:migrate:prod` for production
