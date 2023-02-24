use candid::Encode;
use canister_test::Project;
use ic_base_types::{CanisterId, PrincipalId};
use ic_nns_test_utils::common::{NnsInitPayloads, NnsInitPayloadsBuilder};
use ic_nns_test_utils::state_test_helpers::{self, create_canister, setup_nns_canisters};
use ic_state_machine_tests::StateMachine;

pub const ONE_TRILLION: u128 = 1_000_000_000_000;
pub const EXPECTED_SNS_CREATION_FEE: u128 = 180 * ONE_TRILLION;

/// Create a `StateMachine` with NNS installed
pub fn set_up_state_machine_with_nns(allowed_principals: Vec<PrincipalId>) -> StateMachine {
    // We don't want the underlying warnings of the StateMachine
    state_test_helpers::reduce_state_machine_logging_unless_env_set();
    let machine = StateMachine::new();

    let nns_init_payload = NnsInitPayloadsBuilder::new()
        .with_initial_invariant_compliant_mutations()
        .with_test_neurons()
        .with_sns_dedicated_subnets(machine.get_subnet_ids())
        .with_sns_wasm_access_controls(true)
        .with_sns_wasm_allowed_principals(allowed_principals)
        .build();

    setup_nns_canisters(&machine, nns_init_payload);

    machine
}

pub fn install_sns_wasm(machine: &StateMachine, nns_init_payload: &NnsInitPayloads) -> CanisterId {
    let sns_wasm_bin = Project::cargo_bin_maybe_from_env("sns-wasm-canister", &[]);

    create_canister(
        machine,
        sns_wasm_bin,
        Some(Encode!(&nns_init_payload.sns_wasms.clone()).unwrap()),
        None,
    )
}
