#!/bin/bash
# Test script for GitHub label setup functionality
# This verifies that all Loom workflow labels can be created/updated correctly

set -e

echo "=== Testing GitHub Label Setup ==="
echo

# Check if gh CLI is available
if ! command -v gh &> /dev/null; then
    echo "❌ gh CLI not found. Please install it: https://cli.github.com/"
    exit 1
fi

# Check if we're in a git repository
if ! git rev-parse --git-dir > /dev/null 2>&1; then
    echo "❌ Not in a git repository"
    exit 1
fi

# Check if we have a GitHub remote
if ! git remote -v | grep -q "github.com"; then
    echo "❌ No GitHub remote found"
    exit 1
fi

echo "✅ Prerequisites check passed"
echo

# Test labels (from src/lib/label-setup.ts LOOM_LABELS)
declare -a LABEL_NAMES=("loom:proposal" "loom:hermit" "loom:ready" "loom:in-progress" "loom:blocked" "loom:urgent" "loom:review-requested" "loom:reviewing" "loom:pr")
declare -a LABEL_DESCS=(
  "Architect suggestion awaiting user approval"
  "Critic removal/simplification proposal awaiting user approval"
  "Issue ready for Worker to claim and implement"
  "Worker actively implementing this issue"
  "Implementation blocked, needs help or clarification"
  "High priority issue requiring immediate attention"
  "PR ready for Reviewer to review"
  "Reviewer actively reviewing this PR"
  "PR approved by Reviewer, ready for human to merge"
)
declare -a LABEL_COLORS=("3B82F6" "3B82F6" "10B981" "F59E0B" "EF4444" "DC2626" "10B981" "F59E0B" "3B82F6")

echo "=== Checking Existing Labels ==="
for i in "${!LABEL_NAMES[@]}"; do
    label="${LABEL_NAMES[$i]}"
    if gh label list --json name --jq ".[].name" | grep -q "^${label}$"; then
        echo "✅ $label - exists"
    else
        echo "⚠️  $label - missing"
    fi
done
echo

echo "=== Testing Label Creation (Dry Run) ==="
echo "This would create/update the following labels:"
echo

for i in "${!LABEL_NAMES[@]}"; do
    label="${LABEL_NAMES[$i]}"
    description="${LABEL_DESCS[$i]}"
    color="${LABEL_COLORS[$i]}"
    echo "📝 $label"
    echo "   Description: $description"
    echo "   Color: #$color"
    echo
done

echo "=== Summary ===="
total=${#LABEL_NAMES[@]}
existing=$(gh label list --json name --jq ".[].name" | grep -c "^loom:" || true)
echo "Total labels: $total"
echo "Existing labels: $existing"
echo "Missing labels: $((total - existing))"
echo

echo "✅ Test script completed successfully"
echo
echo "To create/update labels:"
echo "  1. From Loom UI: Use the label setup utility (when implemented)"
echo "  2. Manually: gh label create <name> --description <desc> --color <color> --force"
echo "  3. See WORKFLOWS.md for complete label workflow documentation"
