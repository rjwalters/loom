# Security Policy

## Supported Versions

We are currently in active development. Security updates will be applied to the latest version on the `main` branch.

| Version | Supported          |
| ------- | ------------------ |
| main    | :white_check_mark: |

## Reporting a Vulnerability

If you discover a security vulnerability in Loom, please report it responsibly:

1. **Do NOT** open a public GitHub issue
2. Email the maintainers directly with:
   - Description of the vulnerability
   - Steps to reproduce
   - Potential impact
   - Any suggested fixes (optional)

We will acknowledge your report within 48 hours and provide a timeline for a fix.

## Security Measures

### Automated Scanning

This repository uses several automated tools to detect security issues:

- **Dependabot**: Automatic dependency updates for npm, Cargo, and GitHub Actions
- **Cargo Audit**: Scans Rust dependencies for known security vulnerabilities
- **NPM Audit**: Scans JavaScript dependencies for known security vulnerabilities
- **CodeQL**: Static analysis to detect security issues in JavaScript/TypeScript code
- **Cargo Deny**: Supply chain security checks for Rust dependencies

### Supply Chain Security

- All dependencies are pinned with lock files (`pnpm-lock.yaml`, `Cargo.lock`)
- License compliance is enforced via `deny.toml`
- Multiple versions of the same dependency trigger warnings
- Dependencies must come from trusted sources (crates.io, npm registry)

### Development Practices

- Branch protection requires pull request reviews before merging
- All changes go through CI checks including security scans
- Secrets and credentials are never committed to the repository
- Regular security audits run weekly via GitHub Actions

## Disclosure Policy

When a security issue is reported:

1. We will investigate and validate the report
2. A fix will be developed and tested
3. The fix will be released as soon as possible
4. Credit will be given to the reporter (unless they wish to remain anonymous)
5. A security advisory will be published after the fix is released

## Dependencies

We strive to keep dependencies up-to-date and secure:

- Automated weekly dependency updates via Dependabot
- Security patches are prioritized and merged quickly
- Unmaintained or abandoned dependencies are replaced
- Transitive dependencies are monitored for vulnerabilities

## Contact

For security-related inquiries, please open an issue on GitHub or contact the repository maintainers.

Thank you for helping keep Loom secure!
