# Canonical jq projection for GitHub check states. Keep its table synchronized
# with scripts/classify-required-check.sh and scripts/pr_status.py; focused tests
# exercise all three implementations over the same 2/4/11 state matrix.
def check_outcome:
  ((.conclusion // .state // "") | ascii_upcase) as $conclusion
  | ((.status // "") | ascii_upcase) as $status
  | (($status == "" or $status == "COMPLETED")) as $terminal
  | if $terminal and $conclusion == "SUCCESS" then "PASSED"
    elif $terminal and (["FAILURE", "TIMED_OUT", "ERROR", "STARTUP_FAILURE"]
                        | index($conclusion)) then "FAILED"
    else "NO_RESULT"
    end;
