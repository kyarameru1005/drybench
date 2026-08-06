#!/usr/bin/env bash
# Hook: {{name}}
#
# stdin carries the hook payload as JSON. Exit non-zero to block the action.
# Keep this fast — it runs inline with the tool call.

set -euo pipefail

payload=$(cat)

# TODO: do something with "$payload"
: "$payload"

exit 0
