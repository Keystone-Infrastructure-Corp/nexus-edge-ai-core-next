#!/usr/bin/env bash
# Canonical copy: nexus-home/ci/.github/factory/test-workflows.sh. Copy it verbatim.
# Offline test for the factory workflows in ../workflows. Job `if:` rules are evaluated
# in node against sample events; step `run:` blocks run under `bash -e`, as Actions runs
# them, with a stub gh on PATH. A workflow this repo has not adopted is skipped.
# shellcheck disable=SC2016 # single-quoted JSON and jq programs are literal
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
WF="$HERE/../workflows"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

# The `run: |` block of the step named $2 in workflow $1, de-indented.
step() {
  awk -v want="- name: $2" '
    index($0, want) && substr($0, index($0, want)) == want { f = 1; next }
    f && /^ *run: \|$/ { match($0, /^ */); ind = RLENGTH; r = 1; next }
    r { match($0, /^ */); if ($0 !~ /^ *$/ && RLENGTH <= ind) exit; print substr($0, ind + 3) }
  ' "$1"
}

# The job-level `if: >-` rule in workflow $1, folded onto one line.
job_if() {
  awk '/^    if: >-$/ { f = 1; next } f { if ($0 !~ /^      /) exit; sub(/^ +/, ""); printf "%s ", $0 }' "$1"
}

# Evaluate a GitHub expression as JavaScript against a `github` context. contains()
# ignores case as GitHub's does; == compares exactly, which these rules rely on. A
# missing property throws, which fails the case loudly.
cat >"$TMP/eval.mjs" <<'JS'
const [expr, ctx] = process.argv.slice(2);
const low = (v) => String(v ?? '').toLowerCase();
const contains = (a, b) => (Array.isArray(a) ? a.some((v) => low(v) === low(b)) : low(a).includes(low(b)));
const fromJSON = (s) => JSON.parse(s);
const toJSON = (v) => JSON.stringify(v, null, 2);
const f = new Function('github', 'contains', 'fromJSON', 'toJSON', `return (${expr});`);
console.log(Boolean(f(JSON.parse(ctx), contains, fromJSON, toJSON)));
JS

# rule <workflow> <run|skip> <case> <event_name> <event JSON body>
rule() {
  local ctx got
  ctx="{\"repository\":\"o/r\",\"event_name\":\"$4\",\"event\":{$5}}"
  if ! got=$(node "$TMP/eval.mjs" "$(job_if "$1")" "$ctx" 2>&1); then bad "$3: $got"; return; fi
  if { [ "$2" = run ] && [ "$got" = true ]; } || { [ "$2" = skip ] && [ "$got" = false ]; }; then
    ok "$(basename "$1") if: $3 -> $2"
  else
    bad "$(basename "$1") if: $3 -> got $got, want $2"
  fi
}

# Stub gh: log each call; `pr view` prints GH_FORK (every call fails when GH_FAIL=1);
# `issue view` applies the caller's --jq to GH_LABELS.
mkdir -p "$TMP/bin"
cat >"$TMP/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf 'gh %s\n' "$*" >>"$GH_STUB_LOG"
[ "${GH_FAIL:-0}" = 1 ] && exit 1
jq=""; prev=""
for a in "$@"; do [ "$prev" = --jq ] && jq="$a"; prev="$a"; done
case "$1 $2" in
  "pr view") printf '%s\n' "$GH_FORK" ;;
  "issue view") printf '%s' "{\"labels\":$GH_LABELS}" | jq -r "$jq" ;;
esac
STUB
chmod +x "$TMP/bin/gh"
export PATH="$TMP/bin:$PATH" GH_STUB_LOG="$TMP/gh.log" REPO=o/r PR=5 ISSUE=9
SAME='"pull_request":{"head":{"repo":{"full_name":"o/r"}}}'
FORK='"pull_request":{"head":{"repo":{"full_name":"x/r"}}}'
SMALL='{"name":"factory:small"}'
SPEC='{"name":"spec:SPEC-086"}'
BADSPEC='{"name":"spec:SPEC-1; x"}'

# script <workflow> <step name> <marker>: extract a step to $TMP/step.sh, or fail.
script() {
  step "$1" "$2" >"$TMP/step.sh"
  grep -q -- "$3" "$TMP/step.sh" || { bad "$(basename "$1"): step '$2' not found"; return 1; }
}

C="$WF/claude.yml"
if [ -f "$C" ]; then
  m='"author_association":"MEMBER"'
  rule "$C" run "member @claude on an issue" issue_comment "\"comment\":{\"body\":\"@claude fix it\",$m}"
  rule "$C" skip "outsider @claude" issue_comment '"comment":{"body":"@claude fix it","author_association":"NONE"}'
  rule "$C" skip "member @claude review" issue_comment "\"comment\":{\"body\":\"@claude review\",$m}"
  rule "$C" skip "member comment, no mention" issue_comment "\"comment\":{\"body\":\"looks good\",$m}"
  rule "$C" run "review comment, same repo" pull_request_review_comment "\"comment\":{\"body\":\"@claude why\",$m},$SAME"
  rule "$C" skip "review comment, fork" pull_request_review_comment "\"comment\":{\"body\":\"@claude why\",$m},$FORK"
  rule "$C" skip "review comment @claude review" pull_request_review_comment "\"comment\":{\"body\":\"@claude review\",$m},$SAME"
  rule "$C" run "review body, same repo" pull_request_review "\"review\":{\"body\":\"@claude tidy\",$m},$SAME"
  rule "$C" skip "review body, fork" pull_request_review "\"review\":{\"body\":\"@claude tidy\",$m},$FORK"
  rule "$C" skip "review body @claude review" pull_request_review "\"review\":{\"body\":\"@claude review\",$m},$SAME"
  rule "$C" run "member opens issue with @claude" issues "\"action\":\"opened\",\"issue\":{\"title\":\"x\",\"body\":\"@claude add\",$m}"
  rule "$C" skip "outsider opens issue with @claude" issues '"action":"opened","issue":{"title":"x","body":"@claude add","author_association":"NONE"}'
  rule "$C" run "factory:medium label" issues '"action":"labeled","label":{"name":"factory:medium"}'
  rule "$C" skip "factory:small label" issues '"action":"labeled","label":{"name":"factory:small"}'
  rule "$C" run "factory:fix-ci, same-repo PR" pull_request "\"action\":\"labeled\",\"label\":{\"name\":\"factory:fix-ci\"},$SAME"
  rule "$C" skip "factory:fix-ci, fork PR" pull_request "\"action\":\"labeled\",\"label\":{\"name\":\"factory:fix-ci\"},$FORK"

  # Only a same-repository PR passes the fork guard; anything else stops the job.
  if script "$C" "Refuse pull requests from forks" isCrossRepository; then
    for c in "false pass 0" "true fail 0" "_ fail 0" "false fail 1"; do
      read -r fork want gfail <<<"$c"; [ "$fork" = _ ] && fork=""
      rc=0; GH_FORK="$fork" GH_FAIL="$gfail" bash -e "$TMP/step.sh" >/dev/null 2>&1 || rc=$?
      if { [ "$want" = pass ] && [ "$rc" -eq 0 ]; } || { [ "$want" = fail ] && [ "$rc" -ne 0 ]; }; then
        ok "fork guard: isCrossRepository='$fork' gh_fail=$gfail -> $want"
      else
        bad "fork guard: isCrossRepository='$fork' gh_fail=$gfail -> exit $rc, want $want"
      fi
    done
  fi

  # factory:medium builds only a valid spec; factory:fix-ci loops on the PR branch.
  if script "$C" "Write the build prompt" GITHUB_OUTPUT; then
    for c in "valid|[$SPEC]|0|Run implement-loop for SPEC-086" "none|[]|1|" "malformed|[$BADSPEC]|1|"; do
      IFS='|' read -r name labels want text <<<"$c"
      : >"$TMP/out"; rc=0
      GITHUB_EVENT_NAME=issues GH_LABELS="$labels" GITHUB_OUTPUT="$TMP/out" bash -e "$TMP/step.sh" >/dev/null 2>&1 || rc=$?
      if [ "$((rc != 0))" = "$want" ] && { [ -z "$text" ] || grep -qF "$text" "$TMP/out"; }; then
        ok "medium prompt: $name spec label -> exit $rc"
      else
        bad "medium prompt: $name spec label -> exit $rc, want failure=$want"
      fi
    done
    : >"$TMP/out"
    GITHUB_EVENT_NAME=pull_request BRANCH=fix-x GITHUB_OUTPUT="$TMP/out" bash -e "$TMP/step.sh" >/dev/null 2>&1 || true
    if grep -qF 'implement-loop' "$TMP/out" && grep -qF 'on origin/fix-x' "$TMP/out"; then
      ok "fix-ci prompt runs implement-loop on the PR branch"
    else
      bad "fix-ci prompt lacks implement-loop on origin/fix-x"
    fi
  fi
else
  echo "skip claude.yml (not adopted)"
fi

R="$WF/factory-route.yml"
if [ -f "$R" ]; then
  rule "$R" run "unassigned factory:small" issues '"action":"labeled","label":{"name":"factory:small"},"issue":{"assignees":[]}'
  rule "$R" run "unassigned factory:large" issues '"action":"labeled","label":{"name":"factory:large"},"issue":{"assignees":[]}'
  rule "$R" skip "assigned factory:small" issues '"action":"labeled","label":{"name":"factory:small"},"issue":{"assignees":[{"login":"a"}]}'
  rule "$R" skip "factory:medium (claude.yml)" issues '"action":"labeled","label":{"name":"factory:medium"},"issue":{"assignees":[]}'

  # spec <labels>: run "Read the spec label" and print the SPEC it exported.
  spec() {
    : >"$TMP/env"
    GH_LABELS="$1" GITHUB_ENV="$TMP/env" bash -e "$TMP/spec.sh"
    sed -n 's/^SPEC=//p' "$TMP/env"
  }
  if script "$R" "Read the spec label" GITHUB_ENV; then
    cp "$TMP/step.sh" "$TMP/spec.sh"
    # factory:small assigns Copilot only with a valid spec label and a token.
    if script "$R" "Assign the Copilot cloud agent" assignees; then
      for c in "none|[$SMALL]|x|no" "malformed|[$SMALL,$BADSPEC]|x|no" "valid|[$SMALL,$SPEC]|x|yes" "no token|[$SMALL,$SPEC]||no"; do
        IFS='|' read -r name labels token assign <<<"$c"
        s="$(spec "$labels")"; : >"$GH_STUB_LOG"; rc=0
        SPEC="$s" COPILOT_ASSIGN_TOKEN="$token" bash -e "$TMP/step.sh" >/dev/null 2>&1 || rc=$?
        got=no; grep -q assignees "$GH_STUB_LOG" && got=yes
        commented=no; grep -q 'issue comment' "$GH_STUB_LOG" && commented=yes
        told=no; grep -qF "custom_instructions]=Build $s with the implement-loop skill" "$GH_STUB_LOG" && told=yes
        if [ "$rc" -eq 0 ] && [ "$got" = "$assign" ] && { [ "$assign" = no ] || [ "$told" = yes ]; } &&
           { [ "$assign" = yes ] || [ "$commented" = yes ]; }; then
          ok "small route: $name -> assign=$got"
        else
          bad "small route: $name -> assign=$got commented=$commented exit $rc, want assign=$assign"
        fi
      done
    fi
    # factory:large never builds: it hands the spec to the owner, or says none exists.
    if script "$R" "Hand large builds to the owner" needs:owner; then
      for c in "valid|[$SPEC]|/implement-loop SPEC-086" "none|[]|nothing to build yet"; do
        IFS='|' read -r name labels text <<<"$c"
        s="$(spec "$labels")"; : >"$GH_STUB_LOG"; rc=0
        SPEC="$s" bash -e "$TMP/step.sh" >/dev/null 2>&1 || rc=$?
        if [ "$rc" -eq 0 ] && grep -qF "$text" "$GH_STUB_LOG" && grep -q 'add-label needs:owner' "$GH_STUB_LOG" && ! grep -q assignees "$GH_STUB_LOG"; then
          ok "large route: $name spec label -> owner comment"
        else
          bad "large route: $name spec label -> exit $rc; log: $(tr '\n' ' ' <"$GH_STUB_LOG")"
        fi
      done
    fi
  fi
else
  echo "skip factory-route.yml (not adopted)"
fi

I="$WF/factory-intake.yml"
if [ -f "$I" ]; then
  # Manual runs pass the spec (and a size unless auto); pushes pass their commit range.
  if script "$I" "Open work-order issues" nx-intake; then
    mkdir -p "$TMP/repo" "$TMP/nexus-home/bin"
    printf '#!/usr/bin/env bash\necho "$*" >"%s"\n' "$TMP/args" >"$TMP/nexus-home/bin/nx-intake"
    chmod +x "$TMP/nexus-home/bin/nx-intake"
    for c in "workflow_dispatch|SPEC-086|auto|--spec SPEC-086" \
             "workflow_dispatch|SPEC-086|small|--spec SPEC-086 --size small" \
             "push|||--before b1 --after a1"; do
      IFS='|' read -r ev sp size want <<<"$c"
      : >"$TMP/args"
      (cd "$TMP/repo" && EVENT="$ev" SPEC="$sp" SIZE="$size" BEFORE=b1 AFTER=a1 bash -e "$TMP/step.sh") >/dev/null 2>&1 || true
      if [ "$(cat "$TMP/args")" = "$want" ]; then ok "intake: $ev $size -> $want"; else bad "intake: $ev $size -> '$(cat "$TMP/args")', want '$want'"; fi
    done
  fi
else
  echo "skip factory-intake.yml (not adopted)"
fi

exit "$fail"
