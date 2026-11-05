#!/bin/sh
# Docker-free validation of the critical fixes. Run full crate suites with Docker and ACAN_DEMO_DIR.
set -eu
cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

cargo fmt --all -- --check
cargo check --workspace
cargo test -p acan-llm
cargo test -p acan-agent
cargo test -p acan-core

echo "Skipping witness h01_* suites (Docker and ACAN_DEMO_DIR) and four real-container command tests."
cargo test -p acan-witness --lib --test child_env --test witness -- \
    --skip a_passing_command_passes \
    --skip a_failing_command_fails_with_the_output_tail \
    --skip a_timeout_kills_the_process_tree \
    --skip the_output_tail_is_bounded

echo "Skipping server e2e and demo-backed intent_gate cases (Docker and/or ACAN_DEMO_DIR)."
set -- \
    --skip e2e:: \
    --skip a_candidate_cannot_change_the_protected_inputs \
    --skip a_contrast_with_one_probe_in_flight_and_with_three_has_the_same_receipt \
    --skip a_delivery_read_before_a_cancel_does_not_stand \
    --skip a_delivery_read_before_a_revision_does_not_stand \
    --skip a_head_move_forces_a_fresh_preparation_and_run \
    --skip a_land_installed_before_its_result_was_recorded_is_recorded_as_landed \
    --skip a_live_order_lands_through_the_job_loop \
    --skip a_live_seeded_regression_is_blocked_and_its_receipt_names_the_rule \
    --skip a_missing_protected_path_is_masked_and_the_run_is_judged \
    --skip a_moved_protected_head_rebinds_the_scope_and_the_candidate_is_judged_again \
    --skip a_moved_protected_input_refuses_the_rebind_and_nothing_lands \
    --skip a_passing_order_lands_on_the_protected_branch \
    --skip a_pending_ruling_survives_a_restart_and_is_answered_in_the_log \
    --skip a_probe_that_cannot_start_ends_the_contrast_with_its_error \
    --skip a_restart_between_the_prepared_run_and_its_receipt_neither_repeats_nor_skips_it \
    --skip a_runtime_that_cannot_isolate_a_run_never_installs \
    --skip a_scope_revised_while_queued_never_installs \
    --skip a_seeded_regression_is_blocked_and_the_receipt_names_it
if [ "$(uname -s)" = Darwin ]; then
    echo "Skipping non-UTF-8 filename fixture: macOS rejects creating that filename."
    set -- "$@" --skip a_name_that_is_not_utf8_is_refused_before_any_file_is_written
fi
cargo test -p acan-server -- "$@"
