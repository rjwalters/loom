# Loom Quickstart: Desktop

A modern desktop application template pre-configured for Loom AI-powered development.

## Stack

- **Framework**: Tauri 2.0
- **Frontend**: React 19, TypeScript, Tailwind CSS 4, shadcn/ui
- **Database**: SQLite (via tauri-plugin-sql)
- **Build**: Vite
- **Linting**: Biome

## Features

- System tray with context menu
- Local SQLite database for persistent storage
- Dark/light theme with system preference detection
- Native window management with drag regions
- Responsive layout for window resizing
- Pre-configured Loom roles and workflows

## Prerequisites

- [Node.js](https://nodejs.org/) 18+
- [pnpm](https://pnpm.io/)
- [Rust](https://rustup.rs/)
- Platform-specific dependencies:
  - **macOS**: Xcode Command Line Tools
  - **Windows**: Microsoft Visual C++ Build Tools
  - **Linux**: `webkit2gtk`, `libappindicator3` (see [Tauri docs](https://tauri.app/v2/guides/prerequisites/linux))

## Quick Start

### 1. Copy the template

```bash
# From the Loom repository
cp -r quickstarts/desktop ~/projects/my-app
cd ~/projects/my-app

# Initialize git
git init
git add -A
git commit -m "Initial commit from loom-quickstart-desktop"
```

### 2. Install dependencies

```bash
pnpm install
```

### 3. Start development

```bash
pnpm tauri dev
```

This starts both the Vite dev server and the Tauri development window with hot reload.

## Project Structure

```
├── .loom/
│   ├── roles/
│   │   ├── builder.md      # Tauri-specific build guidance
│   │   └── judge.md        # Desktop app review criteria
│   └── scripts/
│       └── worktree.sh
├── .github/
│   └── labels.yml
├── src-tauri/
│   ├── src/
│   │   ├── main.rs         # Tauri app entry point
│   │   └── commands.rs     # IPC command handlers
│   ├── Cargo.toml
│   └── tauri.conf.json
├── src/
│   ├── components/
│   │   └── ui/             # shadcn/ui components
│   ├── hooks/
│   │   ├── use-theme.tsx   # Theme management
│   │   └── use-database.tsx # SQLite operations
│   ├── pages/
│   │   ├── HomePage.tsx
│   │   ├── NotesPage.tsx
│   │   └── SettingsPage.tsx
│   ├── lib/
│   │   └── utils.ts
│   └── styles/
│       └── globals.css
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
   pnpm tauri dev
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

### Adding Tauri commands

1. Add function in `src-tauri/src/commands.rs`:
   ```rust
   #[tauri::command]
   pub fn my_command(arg: String) -> Result<String, String> {
       Ok(format!("Received: {}", arg))
   }
   ```

2. Register in `src-tauri/src/main.rs`:
   ```rust
   .invoke_handler(tauri::generate_handler![
       commands::greet,
       commands::my_command,  // Add here
   ])
   ```

3. Call from frontend:
   ```typescript
   import { invoke } from "@tauri-apps/api/core";

   const result = await invoke<string>("my_command", { arg: "hello" });
   ```

### Database changes

The SQLite database is initialized in `src/hooks/use-database.tsx`. Modify the schema creation in the `DatabaseProvider` component:

```typescript
await database.execute(`
  CREATE TABLE IF NOT EXISTS my_table (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL
  )
`);
```

### Theming

CSS variables are defined in `src/styles/globals.css`. Modify the `:root` and `.dark` selectors to customize colors.

### System tray

The system tray is configured in `src-tauri/src/main.rs`. Modify the menu items in the `setup` closure.

## Building for Production

### Build for current platform

```bash
pnpm tauri build
```

Outputs will be in `src-tauri/target/release/bundle/`:
- **macOS**: `.app` and `.dmg`
- **Windows**: `.exe` and `.msi`
- **Linux**: `.deb`, `.rpm`, and `.AppImage`

### Cross-platform builds

For cross-compilation, see the [Tauri cross-compilation guide](https://tauri.app/v2/guides/cross-platform-compilation/).

## Scripts

| Script | Description |
|--------|-------------|
| `pnpm dev` | Start Vite dev server only |
| `pnpm build` | Build frontend only |
| `pnpm tauri dev` | Start full Tauri development |
| `pnpm tauri build` | Build production app |
| `pnpm lint` | Check code with Biome |
| `pnpm lint:fix` | Fix linting issues |
| `pnpm test` | Run tests |

## Learn More

- [Loom Documentation](https://github.com/loomhq/loom)
- [Tauri 2.0 Guides](https://tauri.app/v2/guides/)
- [React](https://react.dev)
- [Tailwind CSS](https://tailwindcss.com)
- [shadcn/ui](https://ui.shadcn.com)
