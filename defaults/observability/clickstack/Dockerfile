# Upstream's entry script overwrites an externally supplied session secret.
# Assert the exact pinned bootstrap shape, then remove only that assignment.
# JSON exec form invokes existing image tools; no Loom shell wrapper is added.
FROM clickhouse/clickstack-all-in-one:2.39.1@sha256:b3ff85e093f8dd9da458a2d0c770c6dbafe776adca755c7b91de33528c3b9553
RUN ["node", "-e", "const s=require('fs').readFileSync('/etc/local/entry.base.sh','utf8').split(String.fromCharCode(10)).filter(l=>l.startsWith('export EXPRESS_SESSION_SECRET=')); if(s.length!==1 || s[0]!=='export EXPRESS_SESSION_SECRET=\"hyperdx is cool 👋\"') process.exit(1)"]
RUN ["sed", "-i", "/^export EXPRESS_SESSION_SECRET=/d", "/etc/local/entry.base.sh"]
