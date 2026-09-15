#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — category opt ins.
#
# One slice of the former monolithic tests/hooks/test-guard-destructive.sh,
# split per #7741. Shared fixtures, assertions and catastrophic-phrase payloads
# live in tests/hooks/lib/guard-destructive-harness.sh.
#
# Usage: ./tests/hooks/test-guard-destructive-category-opt-ins.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- SQL DDL/DML opt-out (guards.sqlDdl / LOOM_GUARD_SQL) ---${NC}"
# =========================================================================

# Repo with the SQL guard explicitly disabled via .loom/config.json.
SQL_OFF_REPO=$(make_sql_repo '{"guards":{"sqlDdl":false}}')
# Repo with the SQL guard explicitly enabled via .loom/config.json.
SQL_ON_REPO=$(make_sql_repo '{"guards":{"sqlDdl":true}}')
# Repo whose config has no guards key at all — must default to guard ON.
SQL_ABSENT_REPO=$(make_sql_repo '{"champion":{"auto_merge_max_lines":200}}')
# Repo with malformed config — must fall through to guard ON.
SQL_BAD_REPO=$(make_sql_repo '{ this is not valid json ')

# --- Non-regression: guard ON by default still blocks all five SQL cases ---
assert_deny "SQL default-on: block DROP DATABASE (config guards absent)" \
    "psql -c 'DROP DATABASE mydb;'" "$SQL_ABSENT_REPO"
assert_deny "SQL default-on: block DROP TABLE (config guards absent)" \
    "mysql -e 'DROP TABLE users;'" "$SQL_ABSENT_REPO"
assert_deny "SQL default-on: block DROP SCHEMA (config guards absent)" \
    "psql -c 'DROP SCHEMA public CASCADE;'" "$SQL_ABSENT_REPO"
assert_deny "SQL default-on: block TRUNCATE TABLE (config guards absent)" \
    "psql -c 'TRUNCATE TABLE users;'" "$SQL_ABSENT_REPO"
assert_deny "SQL default-on: block DELETE FROM without WHERE (config guards absent)" \
    "psql -c 'DELETE FROM users;'" "$SQL_ABSENT_REPO"

# --- Non-regression: explicit guards.sqlDdl:true still blocks ---
assert_deny "SQL config-on: block DROP TABLE" \
    "mysql -e 'DROP TABLE users;'" "$SQL_ON_REPO"
assert_deny "SQL config-on: block DELETE FROM without WHERE" \
    "psql -c 'DELETE FROM users;'" "$SQL_ON_REPO"

# --- Non-regression: malformed config falls through to guard ON ---
assert_deny "SQL malformed-config: block DROP TABLE (fall through to on)" \
    "mysql -e 'DROP TABLE users;'" "$SQL_BAD_REPO"
assert_deny "SQL malformed-config: block DELETE FROM without WHERE" \
    "psql -c 'DELETE FROM users;'" "$SQL_BAD_REPO"

# --- Opt-out via config: all five SQL cases pass through as allow ---
assert_allow "SQL config-off: allow DROP DATABASE" \
    "psql -c 'DROP DATABASE mydb;'" "$SQL_OFF_REPO"
assert_allow "SQL config-off: allow DROP TABLE" \
    "mysql -e 'DROP TABLE users;'" "$SQL_OFF_REPO"
assert_allow "SQL config-off: allow DROP SCHEMA" \
    "psql -c 'DROP SCHEMA public CASCADE;'" "$SQL_OFF_REPO"
assert_allow "SQL config-off: allow TRUNCATE TABLE" \
    "psql -c 'TRUNCATE TABLE users;'" "$SQL_OFF_REPO"
assert_allow "SQL config-off: allow DELETE FROM without WHERE" \
    "psql -c 'DELETE FROM users;'" "$SQL_OFF_REPO"

# --- Opt-out must NOT weaken non-SQL guards ---
assert_deny "SQL config-off: rm -rf / still blocked" \
    "rm -rf /" "$SQL_OFF_REPO"
assert_deny "SQL config-off: force-push to main still blocked" \
    "git push --force origin main" "$SQL_OFF_REPO"
assert_deny "SQL config-off: gh repo delete still blocked" \
    "gh repo delete myrepo --yes" "$SQL_OFF_REPO"
# aws ec2 terminate-instances is no longer an ALWAYS_BLOCK deny (#3593); with the
# SQL guard off (cloud guard still on) it is a toggle-gated ask.
assert_ask "SQL config-off: aws ec2 terminate-instances now asks (#3593)" \
    "aws ec2 terminate-instances --instance-ids i-1234" "$SQL_OFF_REPO"
assert_deny "SQL config-off: aws s3 rb still blocked" \
    "aws s3 rb s3://my-bucket --force" "$SQL_OFF_REPO"

# --- Env override: LOOM_GUARD_SQL=0 disables even when config says true ---
assert_allow_env "LOOM_GUARD_SQL=0 overrides config-on: allow DROP TABLE" \
    "LOOM_GUARD_SQL=0" "mysql -e 'DROP TABLE users;'" "$SQL_ON_REPO"
assert_allow_env "LOOM_GUARD_SQL=0 overrides config-on: allow DELETE FROM without WHERE" \
    "LOOM_GUARD_SQL=0" "psql -c 'DELETE FROM users;'" "$SQL_ON_REPO"

# --- Env override: LOOM_GUARD_SQL=1 forces on even when config says false ---
assert_deny_env "LOOM_GUARD_SQL=1 overrides config-off: block DROP TABLE" \
    "LOOM_GUARD_SQL=1" "mysql -e 'DROP TABLE users;'" "$SQL_OFF_REPO"
assert_deny_env "LOOM_GUARD_SQL=1 overrides config-off: block DELETE FROM without WHERE" \
    "LOOM_GUARD_SQL=1" "psql -c 'DELETE FROM users;'" "$SQL_OFF_REPO"

# --- Env override: LOOM_GUARD_SQL=0 still doesn't weaken non-SQL guards ---
assert_deny_env "LOOM_GUARD_SQL=0: rm -rf / still blocked" \
    "LOOM_GUARD_SQL=0" "rm -rf /" "$SQL_ON_REPO"

# Clean up temp repos created above.
for _sql_dir in "$SQL_OFF_REPO" "$SQL_ON_REPO" "$SQL_ABSENT_REPO" "$SQL_BAD_REPO"; do
    [[ -n "$_sql_dir" && "$_sql_dir" != "/" && -d "$_sql_dir/.loom" ]] && rm -rf "$_sql_dir"
done

echo ""

# =========================================================================
echo -e "${YELLOW}--- Cloud CLI opt-out + verb-narrowing (guards.cloudCli / LOOM_GUARD_CLOUD) (#3593) ---${NC}"
# =========================================================================

# --- Verb-narrowing: read-only aws calls no longer prompt (default guard on) ---
assert_allow "Cloud: aws ec2 describe-instances is read-only (allow)" \
    "aws ec2 describe-instances"
assert_allow "Cloud: aws ec2 describe-images is read-only (allow)" \
    "aws ec2 describe-images --owners self"
assert_allow "Cloud: aws s3 ls is read-only (allow)" \
    "aws s3 ls s3://my-bucket"
assert_allow "Cloud: aws lambda list-functions is read-only (allow)" \
    "aws lambda list-functions"
assert_allow "Cloud: aws ec2 get-console-output is read-only (allow)" \
    "aws ec2 get-console-output --instance-id i-1234"

# --- Discoverability: the cloud ASK reason names the guards.cloudCli opt-out (#3604) ---
assert_ask_reason_matches "Cloud: ask reason names guards.cloudCli opt-out (#3604)" \
    "aws ec2 terminate-instances --instance-ids i-1234" "guards\.cloudCli"

# --- Verb-narrowing: mutating aws subcommands still ask (default guard on) ---
assert_ask "Cloud: aws ec2 run-instances asks" \
    "aws ec2 run-instances --image-id ami-123 --count 1"
assert_ask "Cloud: aws ec2 create-volume asks" \
    "aws ec2 create-volume --size 10 --availability-zone us-east-1a"
assert_ask "Cloud: aws ec2 stop-instances asks" \
    "aws ec2 stop-instances --instance-ids i-1234"
assert_ask "Cloud: aws ec2 start-instances asks" \
    "aws ec2 start-instances --instance-ids i-1234"
assert_ask "Cloud: aws ec2 terminate-instances asks (toggle on)" \
    "aws ec2 terminate-instances --instance-ids i-1234"
assert_ask "Cloud: aws s3 cp (mutating) asks" \
    "aws s3 cp ./file s3://my-bucket/file"
assert_ask "Cloud: aws lambda delete-function asks" \
    "aws lambda delete-function --function-name f"
# --- #3595: invoke/publish/copy/assign/mb restored to the mutating verb list ---
# aws lambda invoke executes arbitrary Lambda code with side effects; it is
# neither read-only nor a catastrophic deny, so the pre-#3595 verb-narrowing
# silently un-gated it. Restore the ask (toggle on).
assert_ask "Cloud: aws lambda invoke asks (toggle on, #3595)" \
    "aws lambda invoke --function-name f out.json"
assert_ask "Cloud: aws lambda publish-version asks (#3595)" \
    "aws lambda publish-version --function-name f"
assert_ask "Cloud: aws lambda publish-layer-version asks (#3595)" \
    "aws lambda publish-layer-version --layer-name l --zip-file fileb://l.zip"
assert_ask "Cloud: aws sns publish asks (#3595)" \
    "aws sns publish --topic-arn arn:aws:sns:us-east-1:1:t --message hi"
assert_ask "Cloud: aws ec2 copy-image asks (#3595)" \
    "aws ec2 copy-image --source-image-id ami-123 --source-region us-east-1 --name copy"
assert_ask "Cloud: aws ec2 assign-private-ip-addresses asks (#3595)" \
    "aws ec2 assign-private-ip-addresses --network-interface-id eni-123 --secondary-private-ip-address-count 1"
assert_ask "Cloud: aws s3 mb (make-bucket) asks (#3595)" \
    "aws s3 mb s3://my-new-bucket"
# invoke/publish must NOT re-broaden into read-only false-positives.
assert_allow "Cloud: aws lambda get-function is read-only (allow, #3595)" \
    "aws lambda get-function --function-name f"
assert_allow "Cloud: aws sns list-topics is read-only (allow, #3595)" \
    "aws sns list-topics"

# --- Docker verbs unchanged: mutating asks, read-only allowed (toggle on) ---
# #5823: bare `docker rm` no longer asks (see the dedicated section below); the
# volume-destroying `-v`/`--volumes` variant is what exercises the toggle here.
assert_ask "Cloud: docker rm -v still asks" \
    "docker rm -v my-container"
assert_ask "Cloud: docker stop still asks" \
    "docker stop my-container"
assert_allow "Cloud: docker ps still allowed (read-only)" \
    "docker ps -a"
assert_allow "Cloud: docker logs still allowed (read-only)" \
    "docker logs my-container"

# Repos toggling the cloud guard via .loom/config.json (reuse make_sql_repo — it
# just writes arbitrary config JSON).
CLOUD_OFF_REPO=$(make_sql_repo '{"guards":{"cloudCli":false}}')
CLOUD_ON_REPO=$(make_sql_repo '{"guards":{"cloudCli":true}}')
CLOUD_ABSENT_REPO=$(make_sql_repo '{"champion":{"auto_merge_max_lines":200}}')
CLOUD_BAD_REPO=$(make_sql_repo '{ not valid json ')

# --- Config opt-out: guards.cloudCli:false fully bypasses cloud/docker ASK ---
assert_allow "Cloud config-off: aws ec2 terminate-instances allowed" \
    "aws ec2 terminate-instances --instance-ids i-1234" "$CLOUD_OFF_REPO"
assert_allow "Cloud config-off: aws ec2 run-instances allowed" \
    "aws ec2 run-instances --image-id ami-123" "$CLOUD_OFF_REPO"
assert_allow "Cloud config-off: aws lambda invoke allowed (#3595)" \
    "aws lambda invoke --function-name f out.json" "$CLOUD_OFF_REPO"
assert_allow "Cloud config-off: docker rm -v allowed" \
    "docker rm -v my-container" "$CLOUD_OFF_REPO"

# --- Default-on (absent/malformed config) still asks on mutating cloud calls ---
assert_ask "Cloud config-absent: aws ec2 terminate-instances still asks" \
    "aws ec2 terminate-instances --instance-ids i-1234" "$CLOUD_ABSENT_REPO"
assert_ask "Cloud malformed-config: aws ec2 run-instances still asks" \
    "aws ec2 run-instances --image-id ami-123" "$CLOUD_BAD_REPO"
assert_ask "Cloud config-on: docker rm -v still asks" \
    "docker rm -v my-container" "$CLOUD_ON_REPO"

# --- Env override: LOOM_GUARD_CLOUD=0 bypasses even when config says true ---
assert_allow_env "LOOM_GUARD_CLOUD=0 overrides config-on: aws ec2 terminate allowed" \
    "LOOM_GUARD_CLOUD=0" "aws ec2 terminate-instances --instance-ids i-1234" "$CLOUD_ON_REPO"
assert_allow_env "LOOM_GUARD_CLOUD=0: aws lambda invoke allowed (#3595)" \
    "LOOM_GUARD_CLOUD=0" "aws lambda invoke --function-name f out.json" "$CLOUD_ON_REPO"
assert_allow_env "LOOM_GUARD_CLOUD=0: docker rm -v allowed" \
    "LOOM_GUARD_CLOUD=0" "docker rm -v my-container" "$CLOUD_ON_REPO"

# --- Env override: LOOM_GUARD_CLOUD=1 forces on even when config says false ---
assert_ask_env "LOOM_GUARD_CLOUD=1 overrides config-off: aws ec2 terminate asks" \
    "LOOM_GUARD_CLOUD=1" "aws ec2 terminate-instances --instance-ids i-1234" "$CLOUD_OFF_REPO"
assert_ask_env "LOOM_GUARD_CLOUD=1 overrides config-off: docker rm -v asks" \
    "LOOM_GUARD_CLOUD=1" "docker rm -v my-container" "$CLOUD_OFF_REPO"

# --- Catastrophic denies are NOT gated by the cloud toggle (stay hard denies) ---
assert_deny_env "Cloud toggle off does NOT weaken: aws s3 rb still denied" \
    "LOOM_GUARD_CLOUD=0" "aws s3 rb s3://prod-bucket --force" "$CLOUD_OFF_REPO"
assert_deny_env "Cloud toggle off does NOT weaken: aws s3 rm --recursive still denied" \
    "LOOM_GUARD_CLOUD=0" "aws s3 rm s3://prod-bucket/data --recursive" "$CLOUD_OFF_REPO"
# aws iam delete moved to the UNGATED ask tier (#4216) — it is NOT gated by the
# cloud toggle, so LOOM_GUARD_CLOUD=0 must still ASK (never silently allow). This
# is the whole point of keeping it ungated rather than in CLOUD_ASK_PATTERNS.
assert_ask_env "Cloud toggle off does NOT weaken: aws iam delete-user still ASKS (ungated #4216)" \
    "LOOM_GUARD_CLOUD=0" "aws iam delete-user --user-name bob" "$CLOUD_OFF_REPO"
assert_deny_env "Cloud toggle off does NOT weaken: aws cloudformation delete-stack still denied" \
    "LOOM_GUARD_CLOUD=0" "aws cloudformation delete-stack --stack-name prod" "$CLOUD_OFF_REPO"
assert_deny_env "Cloud toggle off does NOT weaken: docker system prune still denied" \
    "LOOM_GUARD_CLOUD=0" "docker system prune -af" "$CLOUD_OFF_REPO"

# --- Cloud toggle off must NOT weaken non-cloud guards ---
assert_deny_env "Cloud config-off: rm -rf / still blocked" \
    "LOOM_GUARD_CLOUD=0" "rm -rf /" "$CLOUD_OFF_REPO"
assert_deny_env "Cloud config-off: force-push to main still blocked" \
    "LOOM_GUARD_CLOUD=0" "git push --force origin main" "$CLOUD_OFF_REPO"

# Clean up cloud temp repos.
for _cloud_dir in "$CLOUD_OFF_REPO" "$CLOUD_ON_REPO" "$CLOUD_ABSENT_REPO" "$CLOUD_BAD_REPO"; do
    [[ -n "$_cloud_dir" && "$_cloud_dir" != "/" && -d "$_cloud_dir/.loom" ]] && rm -rf "$_cloud_dir"
done

echo ""

# =========================================================================
echo -e "${YELLOW}--- Reversible-GitHub ask opt-in (guards.reversibleGh / LOOM_GUARD_REVERSIBLE_GH) (#3757) ---${NC}"
# =========================================================================
#
# INVERSE polarity of guards.sqlDdl/cloudCli: default OFF (no ask), opted IN.
# gh pr close / gh issue close / gh label delete do not ask by default; they ask
# only when the toggle is enabled. gh release delete is NOT gated by this toggle
# and always asks. Resolution: LOOM_GUARD_REVERSIBLE_GH env > guards.reversibleGh
# config > default false. Reuse make_sql_repo (it only writes .loom/config.json).
REVGH_ON_REPO=$(make_sql_repo '{"guards":{"reversibleGh":true}}')
REVGH_OFF_REPO=$(make_sql_repo '{"guards":{"reversibleGh":false}}')
REVGH_ABSENT_REPO=$(make_sql_repo '{"champion":{"auto_merge_max_lines":200}}')
REVGH_BAD_REPO=$(make_sql_repo '{ not valid json ')

# --- Default OFF: absent key / explicit false / malformed JSON => no ask ---
assert_allow_env "reversibleGh absent key: gh pr close allowed (default off)" \
    "" "gh pr close 42" "$REVGH_ABSENT_REPO"
assert_allow_env "reversibleGh absent key: gh issue close allowed (default off)" \
    "" "gh issue close 100" "$REVGH_ABSENT_REPO"
assert_allow_env "reversibleGh absent key: gh label delete allowed (default off)" \
    "" "gh label delete needs-triage" "$REVGH_ABSENT_REPO"
assert_allow_env "reversibleGh:false config: gh issue close allowed" \
    "" "gh issue close 100" "$REVGH_OFF_REPO"
assert_allow_env "reversibleGh malformed JSON: gh issue close allowed (fails safe to off)" \
    "" "gh issue close 100" "$REVGH_BAD_REPO"

# --- Config ON: guards.reversibleGh:true opts the ask back in ---
assert_ask_env "reversibleGh:true config: gh pr close asks" \
    "" "gh pr close 42" "$REVGH_ON_REPO"
assert_ask_env "reversibleGh:true config: gh issue close asks" \
    "" "gh issue close 100" "$REVGH_ON_REPO"
assert_ask_env "reversibleGh:true config: gh label delete asks" \
    "" "gh label delete needs-triage" "$REVGH_ON_REPO"

# --- Env override wins over config (mirrors sqlDdl/cloudCli precedent) ---
assert_ask_env "LOOM_GUARD_REVERSIBLE_GH=1 overrides config-off: gh issue close asks" \
    "LOOM_GUARD_REVERSIBLE_GH=1" "gh issue close 100" "$REVGH_OFF_REPO"
assert_allow_env "LOOM_GUARD_REVERSIBLE_GH=0 overrides config-on: gh issue close allowed" \
    "LOOM_GUARD_REVERSIBLE_GH=0" "gh issue close 100" "$REVGH_ON_REPO"

# --- gh release delete is NOT gated by this toggle: always asks ---
assert_ask_env "reversibleGh off: gh release delete STILL asks (not gated)" \
    "" "gh release delete v1.0" "$REVGH_OFF_REPO"
assert_ask_env "LOOM_GUARD_REVERSIBLE_GH=0: gh release delete STILL asks (not gated)" \
    "LOOM_GUARD_REVERSIBLE_GH=0" "gh release delete v1.0" "$REVGH_ON_REPO"

# --- Toggle off must NOT weaken unrelated guards ---
assert_ask_env "reversibleGh off: git clean -fd STILL asks (kept in ungated ask tier)" \
    "LOOM_GUARD_REVERSIBLE_GH=0" "git clean -fd" "$REVGH_OFF_REPO"
assert_deny_env "reversibleGh off: rm -rf / still blocked" \
    "LOOM_GUARD_REVERSIBLE_GH=0" "rm -rf /" "$REVGH_OFF_REPO"
assert_deny_env "reversibleGh off: force-push to main still blocked" \
    "LOOM_GUARD_REVERSIBLE_GH=0" "git push --force origin main" "$REVGH_OFF_REPO"

# Clean up reversible-gh temp repos.
for _revgh_dir in "$REVGH_ON_REPO" "$REVGH_OFF_REPO" "$REVGH_ABSENT_REPO" "$REVGH_BAD_REPO"; do
    [[ -n "$_revgh_dir" && "$_revgh_dir" != "/" && -d "$_revgh_dir/.loom" ]] && rm -rf "$_revgh_dir"
done

echo ""

# =========================================================================

print_summary
