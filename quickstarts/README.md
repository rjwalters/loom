# Loom Quickstart Templates

Pre-configured project templates for common use cases, ready for Loom AI-powered development.

## Available Templates

### [webapp](./webapp/)

Modern web application with Cloudflare Workers, Vite, React, Tailwind CSS, and shadcn/ui.

**Features:**
- User authentication (login/logout/register)
- Dark/light theme switching
- Cloudflare D1 database integration
- Pre-configured Loom roles

**Stack:** React 19, TypeScript, Tailwind CSS 4, Cloudflare Pages Functions, D1

```bash
cp -r quickstarts/webapp ~/projects/my-app
cd ~/projects/my-app
pnpm install && pnpm dev
```

## Coming Soon

### desktop (planned)

Desktop application with Tauri, React, and system integration.

**Planned features:**
- Native menu integration
- Auto-updates
- System tray
- Local storage

### api (planned)

API-first backend with Cloudflare Workers and Hono.

**Planned features:**
- Auth middleware
- Rate limiting
- OpenAPI documentation
- KV/D1 integration

## Using Templates

### Option 1: Copy from Loom repository

```bash
# Clone Loom if you haven't
git clone https://github.com/loomhq/loom.git

# Copy template to new location
cp -r loom/quickstarts/webapp ~/projects/my-app

# Initialize as new git repo
cd ~/projects/my-app
rm -rf .git
git init
git add -A
git commit -m "Initial commit from loom-quickstart-webapp"
```

### Option 2: Download directly (future)

```bash
# Future: loom quickstart webapp my-app
```

## Template Structure

Each template includes:

```
template/
├── .loom/
│   ├── roles/          # Loom role definitions
│   │   ├── builder.md  # Customized for this stack
│   │   └── judge.md    # Review guidelines
│   └── scripts/        # Helper scripts
├── .github/
│   └── labels.yml      # Loom workflow labels
├── README.md           # Template documentation
└── ...                 # Stack-specific files
```

## Creating Custom Templates

1. Start with an existing template or create from scratch
2. Add `.loom/roles/` with customized role definitions
3. Add `.github/labels.yml` with Loom labels
4. Include comprehensive README with setup instructions
5. Test the complete workflow (create issue → build → PR → review)

## Contributing

We welcome new templates! To propose a new template:

1. Create an issue with the `loom:architect` label
2. Describe the target use case and stack
3. Outline included features and Loom customizations
4. Once approved, implement following the existing template structure
