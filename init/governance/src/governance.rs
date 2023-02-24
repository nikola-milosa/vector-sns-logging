use crate::pb::v1::{
    add_or_remove_node_provider::Change,
    governance::{
        neuron_in_flight_command::{Command as InFlightCommand, SyncCommand},
        NeuronInFlightCommand,
    },
    governance_error::ErrorType,
    manage_neuron,
    manage_neuron::{
        claim_or_refresh::{By, MemoAndController},
        ClaimOrRefresh, Command, NeuronIdOrSubaccount,
    },
    manage_neuron_response,
    neuron::DissolveState,
    neuron::Followees,
    proposal,
    reward_node_provider::RewardMode,
    settle_community_fund_participation, swap_background_information, Ballot, BallotInfo,
    DerivedProposalInformation, ExecuteNnsFunction, Governance as GovernanceProto, GovernanceError,
    KnownNeuron, KnownNeuronData, ListKnownNeuronsResponse, ListNeurons, ListNeuronsResponse,
    ListProposalInfo, ListProposalInfoResponse, ManageNeuron, ManageNeuronResponse,
    MostRecentMonthlyNodeProviderRewards, Motion, NetworkEconomics, Neuron, NeuronInfo,
    NeuronState, NnsFunction, NodeProvider, OpenSnsTokenSwap, Proposal, ProposalData, ProposalInfo,
    ProposalRewardStatus, ProposalStatus, RewardEvent, RewardNodeProvider, RewardNodeProviders,
    SetSnsTokenSwapOpenTimeWindow, SettleCommunityFundParticipation, SwapBackgroundInformation,
    Tally, Topic, UpdateNodeProvider, Vote,
};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::fmt;
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::string::ToString;

use async_trait::async_trait;
use candid::{Decode, Encode};
use dfn_protobuf::ToProto;
use ic_base_types::{CanisterId, PrincipalId};
use ic_nns_common::pb::v1::{NeuronId, ProposalId};
use ic_nns_common::types::UpdateIcpXdrConversionRatePayload;
use ic_nns_constants::{
    CYCLES_MINTING_CANISTER_ID, GENESIS_TOKEN_CANISTER_ID, GOVERNANCE_CANISTER_ID,
    LIFELINE_CANISTER_ID, REGISTRY_CANISTER_ID, ROOT_CANISTER_ID, SNS_WASM_CANISTER_ID,
};
use ic_protobuf::registry::dc::v1::AddOrRemoveDataCentersProposalPayload;
use ic_sns_root::{GetSnsCanistersSummaryRequest, GetSnsCanistersSummaryResponse};
use ic_sns_swap::pb::v1 as sns_swap_pb;
use ic_sns_wasm::pb::v1::{ListDeployedSnsesRequest, ListDeployedSnsesResponse};
use icp_ledger::{AccountIdentifier, Subaccount, DEFAULT_TRANSFER_FEE};
use registry_canister::mutations::do_add_node_operator::AddNodeOperatorPayload;

#[cfg(target_arch = "wasm32")]
use dfn_core::println;

use crate::pb::v1::governance::GovernanceCachedMetrics;
use crate::pb::v1::manage_neuron_response::{MergeMaturityResponse, StakeMaturityResponse};
use crate::pb::v1::proposal::Action;
use crate::pb::v1::reward_node_provider::RewardToAccount;
use crate::pb::v1::WaitForQuietState;
use cycles_minting_canister::IcpXdrConversionRateCertifiedResponse;
use dfn_candid::candid_one;
use dfn_core::api::spawn;
use ic_crypto_sha::Sha256;
use ic_nervous_system_common::{
    ledger, ledger::IcpLedger, validate_proposal_url, NervousSystemError,
};
use ic_sns_swap::pb::v1::{Lifecycle, RestoreDappControllersRequest};
use icp_ledger::{Tokens, TOKEN_SUBDIVIDABLE_BY};
use registry_canister::pb::v1::NodeProvidersMonthlyXdrRewards;

// A few helper constants for durations.
pub const ONE_DAY_SECONDS: u64 = 24 * 60 * 60;
pub const ONE_YEAR_SECONDS: u64 = (4 * 365 + 1) * ONE_DAY_SECONDS / 4;
pub const ONE_MONTH_SECONDS: u64 = ONE_YEAR_SECONDS / 12;

// The limits on NNS proposal title len (in bytes).
const PROPOSAL_TITLE_BYTES_MIN: usize = 5;
const PROPOSAL_TITLE_BYTES_MAX: usize = 256;
// Proposal validation
// 15000 B
const PROPOSAL_SUMMARY_BYTES_MAX: usize = 15000;
// 2048 characters
const PROPOSAL_URL_CHAR_MAX: usize = 2048;
// 10 characters
const PROPOSAL_URL_CHAR_MIN: usize = 10;
// 70 KB (for executing NNS functions that are not canister upgrades)
const PROPOSAL_EXECUTE_NNS_FUNCTION_PAYLOAD_BYTES_MAX: usize = 70000;

// When wait for quiet is used, a proposal does not need to reach absolute
// majority to be accepted. However there is a minimum amount of votes needed
// for a simple majority to be enough. This minimum is expressed as a ratio of
// the total possible votes for the proposal.
const MIN_NUMBER_VOTES_FOR_PROPOSAL_RATIO: f64 = 0.03;

// Parameter of the wait for quiet algorithm. This is the maximum amount the
// deadline can be delayed on each vote.
pub const WAIT_FOR_QUIET_DEADLINE_INCREASE_SECONDS: u64 = 2 * ONE_DAY_SECONDS;

// 1 KB - maximum payload size of NNS function calls to keep in listing of
// proposals
pub const EXECUTE_NNS_FUNCTION_PAYLOAD_LISTING_BYTES_MAX: usize = 1000;
// 10 KB
pub const PROPOSAL_MOTION_TEXT_BYTES_MAX: usize = 10000;

// The maximum dissolve delay allowed for a neuron.
pub const MAX_DISSOLVE_DELAY_SECONDS: u64 = 8 * ONE_YEAR_SECONDS;

// The age of a neuron that saturates the age bonus for the voting power
// computation.
pub const MAX_NEURON_AGE_FOR_AGE_BONUS: u64 = 4 * ONE_YEAR_SECONDS;

/// The minimum dissolve delay so that a neuron may vote.
pub const MIN_DISSOLVE_DELAY_FOR_VOTE_ELIGIBILITY_SECONDS: u64 = 6 * ONE_MONTH_SECONDS;

/// The maximum number of followees each neuron can establish for each topic.
pub const MAX_FOLLOWEES_PER_TOPIC: usize = 15;

/// The maximum number of recent ballots to keep, per neuron.
pub const MAX_NEURON_RECENT_BALLOTS: usize = 100;

/// The desired period for reward distribution events.
///
/// No two consecutive reward events will happen with less then this duration in
/// between. A reward distribution event will take place as soon as possible
/// once this duration has passed since the last one. Therefore, this is a
/// "desired" period: the actual distribution cannot be guaranteed to be
/// perfectly periodic, and inter-reward-events duration are expected to exceed
/// this desired period by a few seconds.
pub const REWARD_DISTRIBUTION_PERIOD_SECONDS: u64 = ONE_DAY_SECONDS;

/// The maximum number of neurons supported.
pub const MAX_NUMBER_OF_NEURONS: usize = 200_000;

/// The maximum number results returned by the method `list_proposals`.
pub const MAX_LIST_PROPOSAL_RESULTS: u32 = 100;

/// The number of e8s per ICP;
const E8S_PER_ICP: u64 = TOKEN_SUBDIVIDABLE_BY;

/// The max number of unsettled proposals -- that is proposals for which ballots
/// are still stored.
pub const MAX_NUMBER_OF_PROPOSALS_WITH_BALLOTS: usize = 700;

/// The max number of open manage neuron proposals.
pub const MAX_NUMBER_OF_OPEN_MANAGE_NEURON_PROPOSALS: usize = 100000;

/// Max number of hot key for each neuron.
pub const MAX_NUM_HOT_KEYS_PER_NEURON: usize = 10;

const MAX_HEAP_SIZE_IN_KIB: usize = 4 * 1024 * 1024;
const WASM32_PAGE_SIZE_IN_KIB: usize = 64;

/// Max number of wasm32 pages for the heap after which we consider that there
/// is a risk to the ability to grow the heap.
///
/// This is 7/8 of the maximum number of pages. This corresponds to 3.5 GiB.
pub const HEAP_SIZE_SOFT_LIMIT_IN_WASM32_PAGES: usize =
    MAX_HEAP_SIZE_IN_KIB / WASM32_PAGE_SIZE_IN_KIB * 7 / 8;

pub(crate) const LOG_PREFIX: &str = "[Governance] ";

/// Max character length for a neuron's name, in KnownNeuronData.
pub const KNOWN_NEURON_NAME_MAX_LEN: usize = 200;

/// Max character length for the field "description" in KnownNeuronData.
pub const KNOWN_NEURON_DESCRIPTION_MAX_LEN: usize = 3000;

// The number of seconds between automated Node Provider reward events
// Currently 1/12 of a year: 2629800 = 86400 * 365.25 / 12
const NODE_PROVIDER_REWARD_PERIOD_SECONDS: u64 = 2629800;

const VALID_MATURITY_MODULATION_BASIS_POINTS_RANGE: RangeInclusive<i32> = -500..=500;

// The default values for network economics (until we initialize it).
// Can't implement Default since it conflicts with Prost's.
impl NetworkEconomics {
    pub const fn with_default_values() -> Self {
        Self {
            reject_cost_e8s: E8S_PER_ICP,                               // 1 ICP
            neuron_management_fee_per_proposal_e8s: 1_000_000,          // 0.01 ICP
            neuron_minimum_stake_e8s: E8S_PER_ICP,                      // 1 ICP
            neuron_spawn_dissolve_delay_seconds: ONE_DAY_SECONDS * 7,   // 7 days
            maximum_node_provider_rewards_e8s: 1_000_000 * 100_000_000, // 1M ICP
            minimum_icp_xdr_rate: 100,                                  // 1 XDR
            transaction_fee_e8s: DEFAULT_TRANSFER_FEE.get_e8s(),
            max_proposals_to_keep_per_topic: 100,
        }
    }
}

// Utility to transform a subaccount vector, as stored in the protobuf, into an
// optional subaccount.
// If the subaccount vector is empty, returns None.
// If the subaccount vector has exactly 32 bytes return the corresponding array.
// In any other case returns an error.
pub fn subaccount_from_slice(subaccount: &[u8]) -> Result<Option<Subaccount>, GovernanceError> {
    match subaccount.len() {
        0 => Ok(None),
        _ => {
            let arr: [u8; 32] = subaccount.try_into().map_err(|_| {
                GovernanceError::new_with_message(
                    ErrorType::PreconditionFailed,
                    format!(
                        "A slice of length {} bytes cannot be converted to a Subaccount: \
                        subaccounts are exactly 32 bytes in length.",
                        subaccount.len()
                    ),
                )
            })?;
            Ok(Some(Subaccount(arr)))
        }
    }
}

impl GovernanceError {
    pub fn new(error_type: ErrorType) -> Self {
        GovernanceError {
            error_type: error_type as i32,
            ..Default::default()
        }
    }

    pub fn new_with_message(error_type: ErrorType, message: impl ToString) -> Self {
        GovernanceError {
            error_type: error_type as i32,
            error_message: message.to_string(),
        }
    }
}

impl fmt::Display for GovernanceError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}: {}", self.error_type(), self.error_message)
    }
}

impl From<NervousSystemError> for GovernanceError {
    fn from(nervous_system_error: NervousSystemError) -> Self {
        GovernanceError {
            error_type: ErrorType::External as i32,
            error_message: nervous_system_error.error_message,
        }
    }
}

/// Converts a Vote integer enum value into a typed enum value.
impl From<i32> for Vote {
    fn from(vote_integer: i32) -> Vote {
        match Vote::from_i32(vote_integer) {
            Some(v) => v,
            None => {
                println!(
                    "{}Vote::from invoked with unexpected value {}.",
                    LOG_PREFIX, vote_integer
                );
                Vote::Unspecified
            }
        }
    }
}

impl Vote {
    /// Returns whether this vote is eligible for voting reward.
    fn eligible_for_rewards(&self) -> bool {
        match self {
            Vote::Unspecified => false,
            Vote::Yes => true,
            Vote::No => true,
        }
    }
}

impl ManageNeuron {
    pub fn get_neuron_id_or_subaccount(
        &self,
    ) -> Result<Option<NeuronIdOrSubaccount>, GovernanceError> {
        match (self.id.as_ref(), self.neuron_id_or_subaccount.as_ref()) {
            (Some(_), Some(_)) => Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Both id and neuron_id_or_subaccount fields are set",
            )),
            (None, None) => Ok(None),
            (None, Some(id)) => Ok(Some(id.clone())),
            (Some(nid), None) => Ok(Some(NeuronIdOrSubaccount::NeuronId(nid.clone()))),
        }
    }
}

impl NnsFunction {
    /// Returns whether proposals where the action is such an NnsFunction should
    /// be allowed to be submitted when the heap growth potential is low.
    fn allowed_when_resources_are_low(&self) -> bool {
        matches!(
            self,
            NnsFunction::NnsRootUpgrade
                | NnsFunction::NnsCanisterUpgrade
                | NnsFunction::BlessReplicaVersion
                | NnsFunction::UpdateSubnetReplicaVersion
        )
    }
}

impl ManageNeuronResponse {
    pub fn is_err(&self) -> bool {
        matches!(
            &self.command,
            Some(manage_neuron_response::Command::Error(_))
        )
    }

    pub fn err_ref(&self) -> Option<&GovernanceError> {
        match &self.command {
            Some(manage_neuron_response::Command::Error(err)) => Some(err),
            _ => None,
        }
    }

    pub fn err(self) -> Option<GovernanceError> {
        match self.command {
            Some(manage_neuron_response::Command::Error(err)) => Some(err),
            _ => None,
        }
    }

    pub fn is_ok(&self) -> bool {
        !self.is_err()
    }

    pub fn expect(self, msg: &str) -> Self {
        if let Some(manage_neuron_response::Command::Error(err)) = &self.command {
            panic!("{}: {:?}", msg, err);
        }
        self
    }

    pub fn error(err: GovernanceError) -> Self {
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::Error(err)),
        }
    }

    pub fn configure_response() -> Self {
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::Configure(
                manage_neuron_response::ConfigureResponse {},
            )),
        }
    }

    pub fn disburse_response(transfer_block_height: u64) -> Self {
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::Disburse(
                manage_neuron_response::DisburseResponse {
                    transfer_block_height,
                },
            )),
        }
    }

    pub fn spawn_response(created_neuron_id: NeuronId) -> Self {
        let created_neuron_id = Some(created_neuron_id);
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::Spawn(
                manage_neuron_response::SpawnResponse { created_neuron_id },
            )),
        }
    }

    pub fn merge_maturity_response(response: MergeMaturityResponse) -> Self {
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::MergeMaturity(response)),
        }
    }

    pub fn stake_maturity_response(response: StakeMaturityResponse) -> Self {
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::StakeMaturity(response)),
        }
    }

    pub fn follow_response() -> Self {
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::Follow(
                manage_neuron_response::FollowResponse {},
            )),
        }
    }

    pub fn make_proposal_response(proposal_id: ProposalId) -> Self {
        let proposal_id = Some(proposal_id);
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::MakeProposal(
                manage_neuron_response::MakeProposalResponse { proposal_id },
            )),
        }
    }

    pub fn register_vote_response() -> Self {
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::RegisterVote(
                manage_neuron_response::RegisterVoteResponse {},
            )),
        }
    }

    pub fn split_response(created_neuron_id: NeuronId) -> Self {
        let created_neuron_id = Some(created_neuron_id);
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::Split(
                manage_neuron_response::SplitResponse { created_neuron_id },
            )),
        }
    }

    pub fn merge_response() -> Self {
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::Merge(
                manage_neuron_response::MergeResponse {},
            )),
        }
    }

    pub fn disburse_to_neuron_response(created_neuron_id: NeuronId) -> Self {
        let created_neuron_id = Some(created_neuron_id);
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::DisburseToNeuron(
                manage_neuron_response::DisburseToNeuronResponse { created_neuron_id },
            )),
        }
    }

    pub fn claim_or_refresh_neuron_response(refreshed_neuron_id: NeuronId) -> Self {
        let refreshed_neuron_id = Some(refreshed_neuron_id);
        ManageNeuronResponse {
            command: Some(manage_neuron_response::Command::ClaimOrRefresh(
                manage_neuron_response::ClaimOrRefreshResponse {
                    refreshed_neuron_id,
                },
            )),
        }
    }
}

impl NnsFunction {
    pub fn canister_and_function(&self) -> Result<(CanisterId, &str), GovernanceError> {
        let (canister_id, method) = match self {
            NnsFunction::Unspecified => {
                return Err(GovernanceError::new(ErrorType::PreconditionFailed));
            }
            NnsFunction::AssignNoid => (REGISTRY_CANISTER_ID, "add_node_operator"),

            NnsFunction::CreateSubnet => (REGISTRY_CANISTER_ID, "create_subnet"),
            NnsFunction::AddNodeToSubnet => (REGISTRY_CANISTER_ID, "add_nodes_to_subnet"),
            NnsFunction::RemoveNodesFromSubnet => {
                (REGISTRY_CANISTER_ID, "remove_nodes_from_subnet")
            }
            NnsFunction::ChangeSubnetMembership => {
                (REGISTRY_CANISTER_ID, "change_subnet_membership")
            }
            NnsFunction::NnsCanisterInstall => (ROOT_CANISTER_ID, "add_nns_canister"),
            NnsFunction::NnsCanisterUpgrade => (ROOT_CANISTER_ID, "change_nns_canister"),
            NnsFunction::NnsRootUpgrade => (LIFELINE_CANISTER_ID, "upgrade_root"),
            NnsFunction::RecoverSubnet => (REGISTRY_CANISTER_ID, "recover_subnet"),
            NnsFunction::BlessReplicaVersion => (REGISTRY_CANISTER_ID, "bless_replica_version"),
            NnsFunction::RetireReplicaVersion => (REGISTRY_CANISTER_ID, "retire_replica_version"),
            NnsFunction::UpdateNodeOperatorConfig => {
                (REGISTRY_CANISTER_ID, "update_node_operator_config")
            }
            NnsFunction::UpdateSubnetReplicaVersion => {
                (REGISTRY_CANISTER_ID, "update_subnet_replica_version")
            }
            NnsFunction::UpdateConfigOfSubnet => (REGISTRY_CANISTER_ID, "update_subnet"),
            NnsFunction::IcpXdrConversionRate => {
                (CYCLES_MINTING_CANISTER_ID, "set_icp_xdr_conversion_rate")
            }
            NnsFunction::ClearProvisionalWhitelist => {
                (REGISTRY_CANISTER_ID, "clear_provisional_whitelist")
            }
            NnsFunction::SetAuthorizedSubnetworks => {
                (CYCLES_MINTING_CANISTER_ID, "set_authorized_subnetwork_list")
            }
            NnsFunction::SetFirewallConfig => (REGISTRY_CANISTER_ID, "set_firewall_config"),
            NnsFunction::AddFirewallRules => (REGISTRY_CANISTER_ID, "add_firewall_rules"),
            NnsFunction::RemoveFirewallRules => (REGISTRY_CANISTER_ID, "remove_firewall_rules"),
            NnsFunction::UpdateFirewallRules => (REGISTRY_CANISTER_ID, "update_firewall_rules"),
            NnsFunction::StopOrStartNnsCanister => (ROOT_CANISTER_ID, "stop_or_start_nns_canister"),
            NnsFunction::RemoveNodes => (REGISTRY_CANISTER_ID, "remove_nodes"),
            NnsFunction::UninstallCode => (CanisterId::ic_00(), "uninstall_code"),
            NnsFunction::UpdateNodeRewardsTable => {
                (REGISTRY_CANISTER_ID, "update_node_rewards_table")
            }
            NnsFunction::AddOrRemoveDataCenters => {
                (REGISTRY_CANISTER_ID, "add_or_remove_data_centers")
            }
            NnsFunction::UpdateUnassignedNodesConfig => {
                (REGISTRY_CANISTER_ID, "update_unassigned_nodes_config")
            }
            NnsFunction::RemoveNodeOperators => (REGISTRY_CANISTER_ID, "remove_node_operators"),
            NnsFunction::RerouteCanisterRanges => (REGISTRY_CANISTER_ID, "reroute_canister_ranges"),
            NnsFunction::PrepareCanisterMigration => {
                (REGISTRY_CANISTER_ID, "prepare_canister_migration")
            }
            NnsFunction::CompleteCanisterMigration => {
                (REGISTRY_CANISTER_ID, "complete_canister_migration")
            }
            NnsFunction::AddSnsWasm => (SNS_WASM_CANISTER_ID, "add_wasm"),
            NnsFunction::UpdateSubnetType => (CYCLES_MINTING_CANISTER_ID, "update_subnet_type"),
            NnsFunction::ChangeSubnetTypeAssignment => {
                (CYCLES_MINTING_CANISTER_ID, "change_subnet_type_assignment")
            }
            NnsFunction::UpdateAllowedPrincipals => {
                (SNS_WASM_CANISTER_ID, "update_allowed_principals")
            }
            NnsFunction::UpdateSnsWasmSnsSubnetIds => {
                (SNS_WASM_CANISTER_ID, "update_sns_subnet_list")
            }
            NnsFunction::InsertSnsWasmUpgradePathEntries => {
                (SNS_WASM_CANISTER_ID, "insert_upgrade_path_entries")
            }
        };
        Ok((canister_id, method))
    }
}

/// Given two quantities of stake with possible associated age, return the
/// combined stake and the combined age.
pub fn combine_aged_stakes(
    x_stake_e8s: u64,
    x_age_seconds: u64,
    y_stake_e8s: u64,
    y_age_seconds: u64,
) -> (u64, u64) {
    if x_stake_e8s == 0 && y_stake_e8s == 0 {
        (0, 0)
    } else {
        let total_age_seconds: u128 = (x_stake_e8s as u128 * x_age_seconds as u128
            + y_stake_e8s as u128 * y_age_seconds as u128)
            / (x_stake_e8s as u128 + y_stake_e8s as u128);

        // Note that age is adjusted in proportion to the stake, but due to the
        // discrete nature of u64 numbers, some resolution is lost due to the
        // division above. Only if x_age * x_stake is a multiple of y_stake does
        // the age remain constant after this operation. However, in the end, the
        // most that can be lost due to rounding from the actual age, is always
        // less than 1 second, so this is not a problem.
        (x_stake_e8s + y_stake_e8s, total_age_seconds as u64)
    }
}

impl Neuron {
    // --- Utility methods on neurons: mostly not for public consumption.

    /// Returns the state the neuron would be in a time
    /// `now_seconds`. See [NeuronState] for details.
    pub fn state(&self, now_seconds: u64) -> NeuronState {
        if self.spawn_at_timestamp_seconds.is_some() {
            return NeuronState::Spawning;
        }
        match self.dissolve_state {
            Some(DissolveState::DissolveDelaySeconds(d)) => {
                if d > 0 {
                    NeuronState::NotDissolving
                } else {
                    NeuronState::Dissolved
                }
            }
            Some(DissolveState::WhenDissolvedTimestampSeconds(ts)) => {
                if ts > now_seconds {
                    NeuronState::Dissolving
                } else {
                    NeuronState::Dissolved
                }
            }
            None => NeuronState::Dissolved,
        }
    }

    /// Returns true if and only if `principal` is equal to the
    /// controller of this neuron.
    fn is_controlled_by(&self, principal: &PrincipalId) -> bool {
        self.controller.as_ref().map_or(false, |c| c == principal)
    }

    /// Returns true if and only if `principal` is authorized to
    /// perform non-privileged operations, like vote and follow,
    /// on behalf of this neuron, i.e., if `principal` is either the
    /// controller or one of the authorized hot keys.
    fn is_authorized_to_vote(&self, principal: &PrincipalId) -> bool {
        self.is_controlled_by(principal) || self.hot_keys.contains(principal)
    }

    /// Returns true if this is a community fund neuron.
    fn is_community_fund_neuron(&self) -> bool {
        self.joined_community_fund_timestamp_seconds.is_some()
    }

    /// Return the voting power of this neuron.
    ///
    /// The voting power is the stake of the neuron modified by a
    /// bonus of up to 100% depending on the dissolve delay, with
    /// the maximum bonus of 100% received at an 8 year dissolve
    /// delay. The voting power is further modified by the age of
    /// the neuron giving up to 25% bonus after four years.
    pub fn voting_power(&self, now_seconds: u64) -> u64 {
        // We compute the stake adjustments in u128.
        let stake = self.stake_e8s() as u128;
        // Dissolve delay is capped to eight years, but we cap it
        // again here to make sure, e.g., if this changes in the
        // future.
        let d = std::cmp::min(
            self.dissolve_delay_seconds(now_seconds),
            MAX_DISSOLVE_DELAY_SECONDS,
        ) as u128;
        // 'd_stake' is the stake with bonus for dissolve delay.
        let d_stake = stake + ((stake * d) / (MAX_DISSOLVE_DELAY_SECONDS as u128));
        // Sanity check.
        assert!(d_stake <= 2 * stake);
        // The voting power is also a function of the age of the
        // neuron, giving a bonus of up to 25% at the four year mark.
        let a = std::cmp::min(self.age_seconds(now_seconds), MAX_NEURON_AGE_FOR_AGE_BONUS) as u128;
        let ad_stake = d_stake + ((d_stake * a) / (4 * MAX_NEURON_AGE_FOR_AGE_BONUS as u128));
        // Final stake 'ad_stake' is at most 5/4 of the 'd_stake'.
        assert!(ad_stake <= (5 * d_stake) / 4);
        // The final voting power is the stake adjusted by both age
        // and dissolve delay. If the stake is is greater than
        // u64::MAX divided by 2.5, the voting power may actually not
        // fit in a u64.
        std::cmp::min(ad_stake, u64::MAX as u128) as u64
    }

    /// Given the specified `ballots`: determine how this neuron would
    /// vote on a proposal of `topic` based on which neurons this
    /// neuron follows on this topic (or on the default topic if this
    /// neuron doesn't specify any followees for `topic`).
    fn would_follow_ballots(&self, topic: Topic, ballots: &HashMap<u64, Ballot>) -> Vote {
        // Compute the list of followees for this topic. If no
        // following is specified for the topic, use the followees
        // from the 'Unspecified' topic.
        if let Some(followees) = self
            .followees
            .get(&(topic as i32))
            .or_else(|| self.followees.get(&(Topic::Unspecified as i32)))
            // extract plain vector from 'Followees' proto
            .map(|x| &x.followees)
        {
            // If, for some reason, a list of followees is specified
            // but empty (this is not normal), don't vote 'no', as
            // would be the natural result of the algorithm below, but
            // instead don't cast a vote.
            if followees.is_empty() {
                return Vote::Unspecified;
            }
            let mut yes: usize = 0;
            let mut no: usize = 0;
            for f in followees.iter() {
                if let Some(f_vote) = ballots.get(&f.id) {
                    if f_vote.vote == (Vote::Yes as i32) {
                        yes += 1;
                    } else if f_vote.vote == (Vote::No as i32) {
                        no += 1;
                    }
                }
            }
            if 2 * yes > followees.len() {
                return Vote::Yes;
            }
            if 2 * no >= followees.len() {
                return Vote::No;
            }
        }
        // No followees specified.
        Vote::Unspecified
    }

    /// Returns the list of followees on the manage neuron topic for
    /// this neuron.
    fn neuron_managers(&self) -> Option<&Vec<NeuronId>> {
        self.followees
            .get(&(Topic::NeuronManagement as i32))
            .map(|x| &x.followees)
    }

    /// Register that this neuron has cast a ballot for a
    /// proposal. Don't include votes on "real time" topics (such as
    /// setting the ICP/SDR exchange rate) or "private" topics (such
    /// as manage neuron).
    fn register_recent_ballot(&mut self, topic: Topic, proposal_id: &ProposalId, vote: Vote) {
        // Ignore votes on topics for which no public voting history
        // is required.
        if topic == Topic::ExchangeRate || topic == Topic::NeuronManagement {
            return;
        }
        let ballot_info = BallotInfo {
            proposal_id: Some(*proposal_id),
            vote: vote as i32,
        };
        // We would really like to have a circular buffer here. As
        // we're dealing with a simple vector, we insert at the
        // beginning and remove at the end once we have reached
        // the maximum number of votes to keep track of.
        self.recent_ballots.insert(0, ballot_info);
        // Pop and discard elements from the end until we reach
        // the maximum allowed length of the vector.
        while self.recent_ballots.len() > MAX_NEURON_RECENT_BALLOTS {
            self.recent_ballots.pop();
        }
    }

    // See the relevant protobuf for a high-level description of
    // these operations

    /// If this method is called on a non-dissolving neuron, it remains
    /// non-dissolving. If it is called on dissolving neuron, it remains
    /// dissolving.
    ///
    /// If it is called on a dissolved neuron, it becomes non-dissolving and
    /// its 'age' is reset to start counting from when it last entered
    /// the dissolved state, when applicable (that is, the Dissolved state
    /// was reached through explicit dissolution) --- or from `now` when not
    /// applicable (e.g., newly created neuron with zero dissolve delay).
    fn increase_dissolve_delay(
        &mut self,
        now_seconds: u64,
        additional_dissolve_delay_seconds: u32,
    ) -> Result<(), GovernanceError> {
        // TODO(NNS-194).
        let additional_delay = additional_dissolve_delay_seconds as u64;
        if additional_delay == 0 {
            return Err(GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                "Additional delay is 0.",
            ));
        }
        match self.dissolve_state {
            Some(DissolveState::DissolveDelaySeconds(delay)) => {
                let new_delay = std::cmp::min(
                    delay.saturating_add(additional_delay),
                    MAX_DISSOLVE_DELAY_SECONDS,
                );
                // Note that if delay == 0, this neuron was
                // dissolved and it now becomes non-dissolving.
                self.dissolve_state = Some(DissolveState::DissolveDelaySeconds(new_delay));
                if delay == 0 {
                    // We transition from `Dissolved` to `NotDissolving`: reset age.
                    self.aging_since_timestamp_seconds = now_seconds;
                }
                Ok(())
            }
            Some(DissolveState::WhenDissolvedTimestampSeconds(ts)) => {
                if ts > now_seconds {
                    let delay = ts - now_seconds;
                    let new_delay = std::cmp::min(
                        delay.saturating_add(additional_delay),
                        MAX_DISSOLVE_DELAY_SECONDS,
                    );
                    let new_ts = now_seconds + new_delay;
                    // Sanity check:
                    // if additional_delay == 0, then
                    // new_delay == delay == ts - now_seconds, whence
                    // new_ts == now_seconds + ts - now_seconds == ts
                    self.dissolve_state =
                        Some(DissolveState::WhenDissolvedTimestampSeconds(new_ts));
                    // The neuron was and remains `Dissolving`:
                    // its effective neuron age should already be
                    // zero by having an `aging_since` timestamp
                    // in the far future. Reset it just in case.
                    self.aging_since_timestamp_seconds = u64::MAX;
                    Ok(())
                } else {
                    // ts <= now_seconds
                    // This neuron is dissolved. Set it to non-dissolving.
                    let new_delay = std::cmp::min(additional_delay, MAX_DISSOLVE_DELAY_SECONDS);
                    self.dissolve_state = Some(DissolveState::DissolveDelaySeconds(new_delay));
                    // We transition from `Dissolved` to `NotDissolving`: reset age.
                    //
                    // We set the age to ts as, at this point in
                    // time, the neuron exited the dissolving
                    // state and entered the dissolved state.
                    //
                    // This way of setting the age of neuron
                    // transitioning from dissolved to non-dissolving
                    // creates an incentive to increase the
                    // dissolve delay of a dissolved neuron
                    // instead of dissolving it.
                    self.aging_since_timestamp_seconds = ts;
                    Ok(())
                }
            }
            None => {
                // This neuron is dissolved. Set it to non-dissolving.
                let new_delay = std::cmp::min(additional_delay, MAX_DISSOLVE_DELAY_SECONDS);
                self.dissolve_state = Some(DissolveState::DissolveDelaySeconds(new_delay));
                // We transition from `Dissolved` to `NotDissolving`: reset age.
                self.aging_since_timestamp_seconds = now_seconds;
                Ok(())
            }
        }
    }

    /// Join the Internet Computer's community fund. If this neuron is
    /// already a member of the community fund, an error is returned.
    fn join_community_fund(&mut self, now_seconds: u64) -> Result<(), GovernanceError> {
        if self.joined_community_fund_timestamp_seconds.unwrap_or(0) == 0 {
            self.joined_community_fund_timestamp_seconds = Some(now_seconds);
            Ok(())
        } else {
            // Already joined...
            Err(GovernanceError::new(ErrorType::AlreadyJoinedCommunityFund))
        }
    }

    /// Leave the Internet Computer's community fund. If this neuron is not a
    /// member of the community fund, an error will be returned.
    fn leave_community_fund(&mut self) -> Result<(), GovernanceError> {
        if self.joined_community_fund_timestamp_seconds.unwrap_or(0) != 0 {
            self.joined_community_fund_timestamp_seconds = None;
            Ok(())
        } else {
            Err(GovernanceError::new(ErrorType::NotInTheCommunityFund))
        }
    }

    /// If this neuron is not dissolving, start dissolving it.
    ///
    /// If the neuron is dissolving or dissolved, an error is returned.
    fn start_dissolving(&mut self, now_seconds: u64) -> Result<(), GovernanceError> {
        if let Some(DissolveState::DissolveDelaySeconds(delay)) = self.dissolve_state {
            // Neuron is actually not dissolving.
            if delay > 0 {
                self.dissolve_state = Some(DissolveState::WhenDissolvedTimestampSeconds(
                    delay + now_seconds,
                ));
                // When we start dissolving, we set the neuron age to
                // zero, and it stays zero until we stop
                // dissolving. This is represented by setting the
                // 'aging since' to its maximum possible value, which
                // will remain in the future until approximately
                // 292,277,026,596 AD.
                self.aging_since_timestamp_seconds = u64::MAX;
                Ok(())
            } else {
                // Already dissolved - cannot start dissolving.
                Err(GovernanceError::new(ErrorType::RequiresNotDissolving))
            }
        } else {
            // Already dissolving or dissolved - cannot start dissolving.
            Err(GovernanceError::new(ErrorType::RequiresNotDissolving))
        }
    }

    /// If this neuron is dissolving, set it to not dissolving.
    ///
    /// If the neuron is not dissolving, an error is returned.
    fn stop_dissolving(&mut self, now_seconds: u64) -> Result<(), GovernanceError> {
        if let Some(DissolveState::WhenDissolvedTimestampSeconds(ts)) = self.dissolve_state {
            if ts > now_seconds {
                // Dissolve time is in the future: pause dissolving.
                self.dissolve_state = Some(DissolveState::DissolveDelaySeconds(ts - now_seconds));
                self.aging_since_timestamp_seconds = now_seconds;
                Ok(())
            } else {
                // Neuron is already dissolved, so it doesn't
                // make sense to stop dissolving it.
                Err(GovernanceError::new(ErrorType::RequiresDissolving))
            }
        } else {
            // The neuron is not in a dissolving state.
            Err(GovernanceError::new(ErrorType::RequiresDissolving))
        }
    }

    /// Preconditions:
    /// - key to add is not already present in 'hot_keys'
    /// - the key to add is well-formed
    /// - there are not already too many hot keys for this neuron.
    fn add_hot_key(&mut self, new_hot_key: &PrincipalId) -> Result<(), GovernanceError> {
        // Make sure that the same hot key is not added twice.
        for key in &self.hot_keys {
            if *key == *new_hot_key {
                return Err(GovernanceError::new_with_message(
                    ErrorType::HotKey,
                    "Hot key duplicated.",
                ));
            }
        }
        // Allow at most 10 hot keys per neuron.
        if self.hot_keys.len() >= MAX_NUM_HOT_KEYS_PER_NEURON {
            return Err(GovernanceError::new_with_message(
                ErrorType::ResourceExhausted,
                "Reached the maximum number of hotkeys.",
            ));
        }
        self.hot_keys.push(*new_hot_key);
        Ok(())
    }

    /// Precondition: key to remove is present in 'hot_keys'
    fn remove_hot_key(&mut self, hot_key_to_remove: &PrincipalId) -> Result<(), GovernanceError> {
        if let Some(index) = self.hot_keys.iter().position(|x| *x == *hot_key_to_remove) {
            self.hot_keys.swap_remove(index);
            Ok(())
        } else {
            // Hot key to remove was not found.
            Err(GovernanceError::new_with_message(
                ErrorType::NotFound,
                "Remove failed: Hot key not found.",
            ))
        }
    }

    // --- Public interface of a neuron.

    /// Return the age of this neuron.
    ///
    /// A dissolving neuron has age zero.
    ///
    /// Technically, each neuron has an internal `aging_since`
    /// field that is set to the current time when a neuron is
    /// created in a non-dissolving state and reset when a neuron is
    /// not dissolving again after a call to `stop_dissolve`. While a
    /// neuron is dissolving, `aging_since` is a value in the far
    /// future, effectively making its age zero.
    pub fn age_seconds(&self, now_seconds: u64) -> u64 {
        now_seconds.saturating_sub(self.aging_since_timestamp_seconds)
    }

    /// Returns the dissolve delay of this neuron. For a non-dissolving
    /// neuron, this is just the recorded dissolve delay; for a
    /// dissolving neuron, this is the the time left (from
    /// `now_seconds`) until the neuron becomes dissolved; for a
    /// dissolved neuron, this function returns zero.
    pub fn dissolve_delay_seconds(&self, now_seconds: u64) -> u64 {
        match self.dissolve_state {
            Some(DissolveState::DissolveDelaySeconds(d)) => d,
            Some(DissolveState::WhenDissolvedTimestampSeconds(ts)) => {
                ts.saturating_sub(now_seconds)
            }
            None => 0,
        }
    }

    pub fn is_dissolved(&self, now_seconds: u64) -> bool {
        self.dissolve_delay_seconds(now_seconds) == 0
    }

    /// Apply the specified neuron configuration operation on this neuron.
    ///
    /// See [manage_neuron::Configure] for details.
    pub fn configure(
        &mut self,
        caller: &PrincipalId,
        now_seconds: u64,
        cmd: &manage_neuron::Configure,
    ) -> Result<(), GovernanceError> {
        // This group of methods can only be invoked by the
        // controller of the neuron.
        if !self.is_controlled_by(caller) {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                format!(
                    "Caller '{:?}' must be the controller of the neuron to perform this operation.",
                    caller,
                ),
            ));
        }
        let op = &cmd.operation.as_ref().ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                "Configure must have an operation.",
            )
        })?;
        match op {
            manage_neuron::configure::Operation::IncreaseDissolveDelay(d) => {
                self.increase_dissolve_delay(now_seconds, d.additional_dissolve_delay_seconds)
            }
            manage_neuron::configure::Operation::SetDissolveTimestamp(d) => {
                if now_seconds > d.dissolve_timestamp_seconds {
                    return Err(GovernanceError::new_with_message(
                        ErrorType::InvalidCommand,
                        "The dissolve delay must be set to a future time.",
                    ));
                }
                let desired_dd = d.dissolve_timestamp_seconds - now_seconds;
                let current_dd = self.dissolve_delay_seconds(now_seconds);

                if current_dd > desired_dd {
                    return Err(GovernanceError::new_with_message(
                        ErrorType::InvalidCommand,
                        "Can't set a dissolve delay that is smaller than the current dissolve delay."
                    ));
                }

                let dd_diff = desired_dd - current_dd;
                self.increase_dissolve_delay(
                    now_seconds,
                    dd_diff.try_into().map_err(|_| {
                        GovernanceError::new_with_message(
                            ErrorType::InvalidCommand,
                            "Can't convert u64 dissolve delay into u32.",
                        )
                    })?,
                )
            }
            manage_neuron::configure::Operation::StartDissolving(_) => {
                self.start_dissolving(now_seconds)
            }
            manage_neuron::configure::Operation::StopDissolving(_) => {
                self.stop_dissolving(now_seconds)
            }
            manage_neuron::configure::Operation::AddHotKey(k) => {
                let hot_key = k.new_hot_key.as_ref().ok_or_else(|| {
                    GovernanceError::new_with_message(
                    ErrorType::InvalidCommand,
                    "Operation AddHotKey requires the hot key to add to be specified in the input",
                )
                })?;
                self.add_hot_key(hot_key)
            }
            manage_neuron::configure::Operation::RemoveHotKey(k) => {
                let hot_key = k.hot_key_to_remove.as_ref().ok_or_else(|| GovernanceError::new_with_message(
                    ErrorType::InvalidCommand,
                    "Operation RemoveHotKey requires the hot key to remove to be specified in the input",
                ))?;
                self.remove_hot_key(hot_key)
            }
            manage_neuron::configure::Operation::JoinCommunityFund(_) => {
                self.join_community_fund(now_seconds)
            }
            manage_neuron::configure::Operation::LeaveCommunityFund(_) => {
                self.leave_community_fund()
            }
            manage_neuron::configure::Operation::ChangeAutoStakeMaturity(change) => {
                if change.requested_setting_for_auto_stake_maturity {
                    self.auto_stake_maturity = Some(true);
                } else {
                    self.auto_stake_maturity = None;
                }
                Ok(())
            }
        }
    }

    /// Get the 'public' information associated with this neuron.
    pub fn get_neuron_info(&self, now_seconds: u64) -> NeuronInfo {
        NeuronInfo {
            retrieved_at_timestamp_seconds: now_seconds,
            state: self.state(now_seconds) as i32,
            age_seconds: self.age_seconds(now_seconds),
            dissolve_delay_seconds: self.dissolve_delay_seconds(now_seconds),
            recent_ballots: self.recent_ballots.clone(),
            voting_power: self.voting_power(now_seconds),
            created_timestamp_seconds: self.created_timestamp_seconds,
            stake_e8s: self.minted_stake_e8s(),
            joined_community_fund_timestamp_seconds: self.joined_community_fund_timestamp_seconds,
            known_neuron_data: self.known_neuron_data.as_ref().cloned(),
        }
    }

    /// Return the current 'stake' of this Neuron in number of 10^-8 ICPs.
    /// (That is, if the stake is 1 ICP, this function will return 10^8).
    ///
    /// The stake can be decreased by making proposals that are
    /// subsequently rejected, and increased by transferring funds
    /// to the acccount of this neuron and then refreshing the stake, or
    /// by accumulating staked maturity.
    pub fn stake_e8s(&self) -> u64 {
        self.cached_neuron_stake_e8s
            .saturating_sub(self.neuron_fees_e8s)
            .saturating_add(self.staked_maturity_e8s_equivalent.unwrap_or(0))
    }

    /// Returns the current `minted` stake of the neuron, i.e. the ICP backing the
    /// neuron, minus the fees. This does not count staked maturity.
    pub fn minted_stake_e8s(&self) -> u64 {
        self.cached_neuron_stake_e8s
            .saturating_sub(self.neuron_fees_e8s)
    }

    /// Set the cached stake of this neuron to `updated_stake_e8s` and adjust
    /// this neuron's age accordingly.
    pub fn update_stake(&mut self, updated_stake_e8s: u64, now: u64) {
        // If the updated stake is less than the original stake, preserve the
        // age and distribute it over the new amount. This should not happen
        // in practice, so this code exists merely as a defensive fallback.
        //
        // TODO(NNS1-954) Consider whether update_stake (and other similar
        // methods) should use a neurons effective stake rather than the
        // cached stake.
        if updated_stake_e8s < self.cached_neuron_stake_e8s {
            println!(
                "{}Reducing neuron {:?} stake via update_stake: {} -> {}",
                LOG_PREFIX, self.id, self.cached_neuron_stake_e8s, updated_stake_e8s
            );
            self.cached_neuron_stake_e8s = updated_stake_e8s;
        } else {
            // If one looks at "stake * age" as describing an area, the goal
            // at this point is to increase the stake while keeping the area
            // constant. This means decreasing the age in proportion to the
            // additional stake, which is the purpose of combine_aged_stakes.
            let (new_stake_e8s, new_age_seconds) = combine_aged_stakes(
                self.cached_neuron_stake_e8s,
                self.age_seconds(now),
                updated_stake_e8s.saturating_sub(self.cached_neuron_stake_e8s),
                0,
            );
            // A consequence of the math above is that the 'new_stake_e8s' is
            // always the same as the 'updated_stake_e8s'. We use
            // 'combine_aged_stakes' here to make sure the age is
            // appropriately pro-rated to accommodate the new stake.
            assert!(new_stake_e8s == updated_stake_e8s);
            self.cached_neuron_stake_e8s = new_stake_e8s;
            self.aging_since_timestamp_seconds = now.saturating_sub(new_age_seconds);
        }
    }
}

impl Proposal {
    /// Whether this proposal is restricted, that is, whether neuron voting
    /// eligibility depends on the content of this proposal.
    pub fn is_manage_neuron(&self) -> bool {
        self.topic() == Topic::NeuronManagement
    }

    /// If this is a [ManageNeuron] proposal, this returns the ID of
    /// the managed neuron.
    pub fn managed_neuron(&self) -> Option<NeuronIdOrSubaccount> {
        if let Some(action) = &self.action {
            match action {
                proposal::Action::ManageNeuron(n) => n
                    .get_neuron_id_or_subaccount()
                    .expect("Validation of managed neuron failed"),
                _ => None,
            }
        } else {
            None
        }
    }

    /// Compute the topic that a given proposal belongs to. The topic
    /// of a proposal governs what followers that are taken into
    /// account when the proposal is voted on.
    pub(crate) fn topic(&self) -> Topic {
        if let Some(action) = &self.action {
            match action {
                proposal::Action::ManageNeuron(_) => Topic::NeuronManagement,
                proposal::Action::ManageNetworkEconomics(_) => Topic::NetworkEconomics,
                proposal::Action::Motion(_) => Topic::Governance,
                proposal::Action::ApproveGenesisKyc(_) => Topic::Kyc,
                proposal::Action::ExecuteNnsFunction(m) => {
                    if let Some(mt) = NnsFunction::from_i32(m.nns_function) {
                        match mt {
                            NnsFunction::Unspecified => Topic::Unspecified,
                            NnsFunction::AssignNoid
                            | NnsFunction::UpdateNodeOperatorConfig
                            | NnsFunction::RemoveNodeOperators
                            | NnsFunction::RemoveNodes
                            | NnsFunction::UpdateUnassignedNodesConfig => Topic::NodeAdmin,
                            NnsFunction::CreateSubnet
                            | NnsFunction::AddNodeToSubnet
                            | NnsFunction::RecoverSubnet
                            | NnsFunction::RemoveNodesFromSubnet
                            | NnsFunction::ChangeSubnetMembership
                            | NnsFunction::UpdateConfigOfSubnet => Topic::SubnetManagement,
                            NnsFunction::BlessReplicaVersion
                            | NnsFunction::RetireReplicaVersion => Topic::ReplicaVersionManagement,
                            NnsFunction::UpdateSubnetReplicaVersion => {
                                Topic::SubnetReplicaVersionManagement
                            }
                            NnsFunction::NnsCanisterInstall
                            | NnsFunction::NnsCanisterUpgrade
                            | NnsFunction::NnsRootUpgrade
                            | NnsFunction::StopOrStartNnsCanister
                            | NnsFunction::AddSnsWasm
                            | NnsFunction::InsertSnsWasmUpgradePathEntries => {
                                Topic::NetworkCanisterManagement
                            }
                            NnsFunction::IcpXdrConversionRate => Topic::ExchangeRate,
                            NnsFunction::ClearProvisionalWhitelist => Topic::NetworkEconomics,
                            NnsFunction::SetAuthorizedSubnetworks => Topic::SubnetManagement,
                            NnsFunction::SetFirewallConfig => Topic::SubnetManagement,
                            NnsFunction::AddFirewallRules => Topic::SubnetManagement,
                            NnsFunction::RemoveFirewallRules => Topic::SubnetManagement,
                            NnsFunction::UpdateFirewallRules => Topic::SubnetManagement,
                            NnsFunction::UninstallCode => Topic::Governance,
                            NnsFunction::UpdateNodeRewardsTable => Topic::NetworkEconomics,
                            NnsFunction::AddOrRemoveDataCenters => Topic::ParticipantManagement,
                            NnsFunction::RerouteCanisterRanges => Topic::SubnetManagement,
                            NnsFunction::PrepareCanisterMigration => Topic::SubnetManagement,
                            NnsFunction::CompleteCanisterMigration => Topic::SubnetManagement,
                            NnsFunction::UpdateSubnetType => Topic::SubnetManagement,
                            NnsFunction::ChangeSubnetTypeAssignment => Topic::SubnetManagement,
                            NnsFunction::UpdateAllowedPrincipals => Topic::SnsAndCommunityFund,
                            NnsFunction::UpdateSnsWasmSnsSubnetIds => Topic::SubnetManagement,
                        }
                    } else {
                        Topic::Unspecified
                    }
                }
                proposal::Action::AddOrRemoveNodeProvider(_) => Topic::ParticipantManagement,
                proposal::Action::RewardNodeProvider(_)
                | proposal::Action::RewardNodeProviders(_) => Topic::NodeProviderRewards,
                proposal::Action::SetDefaultFollowees(_)
                | proposal::Action::RegisterKnownNeuron(_) => Topic::Governance,
                proposal::Action::SetSnsTokenSwapOpenTimeWindow(action) => {
                    println!(
                        "{}ERROR: Obsolete proposal type used: {:?}",
                        LOG_PREFIX, action
                    );
                    Topic::SnsAndCommunityFund
                }
                proposal::Action::OpenSnsTokenSwap(_) => Topic::SnsAndCommunityFund,
            }
        } else {
            Topic::Unspecified
        }
    }

    /// Returns whether such a proposal should be allowed to
    /// be submitted when the heap growth potential is low.
    fn allowed_when_resources_are_low(&self) -> bool {
        self.action
            .as_ref()
            .map_or(false, |a| a.allowed_when_resources_are_low())
    }
}

impl Action {
    /// Returns whether proposals with such an action should be allowed to
    /// be submitted when the heap growth potential is low.
    fn allowed_when_resources_are_low(&self) -> bool {
        match &self {
            proposal::Action::ExecuteNnsFunction(update) => {
                match NnsFunction::from_i32(update.nns_function) {
                    Some(f) => f.allowed_when_resources_are_low(),
                    None => false,
                }
            }
            _ => false,
        }
    }
}

impl ProposalData {
    pub fn topic(&self) -> Topic {
        if let Some(proposal) = &self.proposal {
            proposal.topic()
        } else {
            Topic::Unspecified
        }
    }

    /// Compute the 'status' of a proposal. See [ProposalStatus] for
    /// more information.
    pub fn status(&self) -> ProposalStatus {
        if self.decided_timestamp_seconds == 0 {
            ProposalStatus::Open
        } else if self.is_accepted() {
            if self.executed_timestamp_seconds > 0 {
                ProposalStatus::Executed
            } else if self.failed_timestamp_seconds > 0 {
                ProposalStatus::Failed
            } else {
                ProposalStatus::Adopted
            }
        } else {
            ProposalStatus::Rejected
        }
    }

    /// Whether this proposal is restricted, that is, whether neuron voting
    /// eligibility depends on the content of this proposal.
    pub fn is_manage_neuron(&self) -> bool {
        self.proposal
            .as_ref()
            .map_or(false, Proposal::is_manage_neuron)
    }

    pub fn reward_status(
        &self,
        now_seconds: u64,
        voting_period_seconds: u64,
    ) -> ProposalRewardStatus {
        if self.is_manage_neuron() {
            return ProposalRewardStatus::Ineligible;
        }
        match self.reward_event_round {
            0 => {
                if self.accepts_vote(now_seconds, voting_period_seconds) {
                    ProposalRewardStatus::AcceptVotes
                } else {
                    ProposalRewardStatus::ReadyToSettle
                }
            }
            _ => ProposalRewardStatus::Settled,
        }
    }

    pub fn get_deadline_timestamp_seconds(&self, voting_period_seconds: u64) -> u64 {
        if let Some(wait_for_quiet_state) = &self.wait_for_quiet_state {
            wait_for_quiet_state.current_deadline_timestamp_seconds
        } else {
            self.proposal_timestamp_seconds
                .saturating_add(voting_period_seconds)
        }
    }

    /// Returns true if votes are still accepted for this proposal and
    /// false otherwise.
    ///
    /// For voting reward purposes, votes may be accepted even after a
    /// decision has been made on a proposal. Such votes will not
    /// affect the decision on the proposal, but they affect the
    /// voting rewards of the voting neuron.
    ///
    /// This, this method can return true even if the proposal is
    /// already decided.
    pub fn accepts_vote(&self, now_seconds: u64, voting_period_seconds: u64) -> bool {
        // Naive version of the wait-for-quiet mechanics. For now just tests
        // that the proposal duration is smaller than the threshold, which
        // we're just currently setting as seconds.
        //
        // Wait for quiet is meant to be able to decide proposals without
        // quorum. The tally must have been done above already.
        //
        // If the wait for quit threshold is unset (0), then proposals can
        // accept votes forever.
        now_seconds < self.get_deadline_timestamp_seconds(voting_period_seconds)
    }

    pub fn evaluate_wait_for_quiet(
        &mut self,
        now_seconds: u64,
        voting_period_seconds: u64,
        old_tally: &Tally,
        new_tally: &Tally,
    ) {
        let wait_for_quiet_state = match self.wait_for_quiet_state.as_mut() {
            Some(wait_for_quiet_state) => wait_for_quiet_state,
            None => return,
        };

        // Dont evaluate wait for quiet if there is already a decision, or the
        // deadline has been met. The deciding amount for yes and no are
        // slightly different, because yes needs a majority to succeed, while
        // no only needs a tie.
        let current_deadline = wait_for_quiet_state.current_deadline_timestamp_seconds;
        let deciding_amount_yes = new_tally.total / 2 + 1;
        let deciding_amount_no = (new_tally.total + 1) / 2;
        if new_tally.yes >= deciding_amount_yes
            || new_tally.no >= deciding_amount_no
            || now_seconds > current_deadline
        {
            return;
        }

        // Returns whether the vote has turned, i.e. if the vote is now yes, when it was
        // previously no, or if the vote is now no if it was previously yes.
        fn vote_has_turned(old_tally: &Tally, new_tally: &Tally) -> bool {
            (old_tally.yes > old_tally.no && new_tally.yes <= new_tally.no)
                || (old_tally.yes <= old_tally.no && new_tally.yes > new_tally.no)
        }
        if !vote_has_turned(old_tally, new_tally) {
            return;
        }

        // The required_margin reflects the proposed deadline extension to be
        // made beyond the current moment, so long as that extends beyond the
        // current wait-for-quiet deadline. We calculate the required_margin a
        // bit indirectly here so as to keep with unsigned integers, but the
        // idea is:
        //
        //     W + (voting_period - elapsed) / 2
        //
        // Thus, while we are still within the original voting period, we add
        // to W, but once we are beyond that window, we subtract from W until
        // reaching the limit where required_margin remains at zero. This
        // occurs when:
        //
        //     elapsed = voting_period + 2 * W
        //
        // As an example, given that W = 12h, if the initial voting_period is
        // 24h then the maximum deadline will be 48h.
        //
        // The required_margin ends up being a linearly decreasing value,
        // starting at W + voting_period / 2 and reducing to zero at the
        // furthest possible deadline. When the vote does not flip, we do not
        // update the deadline, and so there is a chance of ending prior to
        // the extreme limit. But each time the vote flips, we "re-enter" the
        // linear progression according to the elapsed time.
        //
        // This means that whenever there is a flip, the deadline is always
        // set to the current time plus the required_margin, which places us
        // along the a linear path that was determined by the starting
        // variables.
        let elapsed_seconds = now_seconds.saturating_sub(self.proposal_timestamp_seconds);
        let required_margin = WAIT_FOR_QUIET_DEADLINE_INCREASE_SECONDS
            .saturating_add(voting_period_seconds / 2)
            .saturating_sub(elapsed_seconds / 2);
        let new_deadline = std::cmp::max(
            current_deadline,
            now_seconds.saturating_add(required_margin),
        );

        if new_deadline != current_deadline {
            println!(
                "{}Updating WFQ deadline for proposal: {:?}. Old: {}, New: {}, Ext: {}",
                LOG_PREFIX,
                self.id.unwrap(),
                current_deadline,
                new_deadline,
                new_deadline - current_deadline
            );

            wait_for_quiet_state.current_deadline_timestamp_seconds = new_deadline;
        }
    }

    /// This is an expensive operation.
    pub fn recompute_tally(&mut self, now_seconds: u64, voting_period_seconds: u64) {
        // Tally proposal
        let mut yes = 0;
        let mut no = 0;
        let mut undecided = 0;
        for ballot in self.ballots.values() {
            let lhs: &mut u64 = if let Some(vote) = Vote::from_i32(ballot.vote) {
                match vote {
                    Vote::Unspecified => &mut undecided,
                    Vote::Yes => &mut yes,
                    Vote::No => &mut no,
                }
            } else {
                &mut undecided
            };
            *lhs = (*lhs).saturating_add(ballot.voting_power)
        }

        // It is validated in `make_proposal` that the total does not
        // exceed u64::MAX: the `saturating_add` is just a precaution.
        let total = yes.saturating_add(no).saturating_add(undecided);

        let new_tally = Tally {
            timestamp_seconds: now_seconds,
            yes,
            no,
            total,
        };

        // Every time the tally changes, (possibly) update the wait-for-quiet
        // dynamic deadline.
        if let Some(old_tally) = self.latest_tally.clone() {
            if new_tally.yes == old_tally.yes
                && new_tally.no == old_tally.no
                && new_tally.total == old_tally.total
            {
                return;
            }

            self.evaluate_wait_for_quiet(
                now_seconds,
                voting_period_seconds,
                &old_tally,
                &new_tally,
            );
        }

        self.latest_tally = Some(new_tally);
    }

    /// Returns true if a proposal meets the conditions to be accepted. The
    /// result is only meaningful if the deadline has passed.
    pub fn is_accepted(&self) -> bool {
        if let Some(tally) = self.latest_tally.as_ref() {
            if self.wait_for_quiet_state.is_none() {
                tally.is_absolute_majority_for_yes()
            } else {
                (tally.yes as f64 >= tally.total as f64 * MIN_NUMBER_VOTES_FOR_PROPOSAL_RATIO)
                    && tally.yes > tally.no
            }
        } else {
            false
        }
    }

    /// Returns true if a decision may be made right now to adopt or
    /// reject this proposal. The proposal must be tallied prior to
    /// calling this method.
    pub(crate) fn can_make_decision(&self, now_seconds: u64, voting_period_seconds: u64) -> bool {
        if let Some(tally) = &self.latest_tally {
            // A proposal is adopted if strictly more than half of the
            // votes are 'yes' and rejected if at least half of the votes
            // are 'no'. The conditions are described as below to avoid
            // overflow. In the absence of overflow, the below is
            // equivalent to (2 * yes > total) || (2 * no >= total).
            let majority =
                (tally.yes > tally.total - tally.yes) || (tally.no >= tally.total - tally.no);
            let expired = !self.accepts_vote(now_seconds, voting_period_seconds);
            let decision_reason = match (majority, expired) {
                (true, true) => Some("majority and expiration"),
                (true, false) => Some("majority"),
                (false, true) => Some("expiration"),
                (false, false) => None,
            };
            if let Some(reason) = decision_reason {
                println!(
                    "{}Proposal {} decided, thanks to {}. Tally at decision time: {:?}",
                    LOG_PREFIX,
                    self.id
                        .map_or("unknown".to_string(), |i| format!("{}", i.id)),
                    reason,
                    tally
                );
                return true;
            }
        }
        false
    }

    /// Return true if this proposal can be purged from storage, e.g.,
    /// if it is allowed to be garbage collected.
    pub fn can_be_purged(&self, now_seconds: u64, voting_period_seconds: u64) -> bool {
        if !self.status().is_final() {
            return false;
        }

        if !self
            .reward_status(now_seconds, voting_period_seconds)
            .is_final()
        {
            return false;
        }

        if let Some(Action::OpenSnsTokenSwap(_)) =
            self.proposal.as_ref().and_then(|p| p.action.as_ref())
        {
            return self.open_sns_token_swap_can_be_purged();
        }

        true
    }

    // Precondition: action must be OpenSnsTokenSwap (behavior is undefined otherwise).
    //
    // The idea here is that we must wait until Community Fund participation has
    // been settled (part of swap finalization), because in that case, we are
    // holding CF participation in escrow.
    //
    // We can tell whether CF participation settlement has been taken care of by
    // looking at the sns_token_swap_lifecycle field.
    fn open_sns_token_swap_can_be_purged(&self) -> bool {
        match self.status() {
            ProposalStatus::Rejected => {
                // Because nothing has been taken from the community fund yet (and never
                // will). We handle this specially, because in this case,
                // sns_token_swap_lifecycle will be None, which is later treated as not
                // terminal.
                true
            }

            ProposalStatus::Failed => {
                // Because because maturity is refunded to the Community Fund before setting
                // execution status to failed.
                true
            }

            ProposalStatus::Executed => {
                // Need to wait for settle_community_fund_participation.
                self.sns_token_swap_lifecycle
                    .and_then(sns_swap_pb::Lifecycle::from_i32)
                    .unwrap_or(sns_swap_pb::Lifecycle::Unspecified)
                    .is_terminal()
            }

            status => {
                println!(
                    "{}WARNING: Proposal status unexpectedly {:?}. self={:#?}",
                    LOG_PREFIX, status, self,
                );
                false
            }
        }
    }

    fn set_sale_lifecycle_by_settle_cf_request_type(
        &mut self,
        result: &settle_community_fund_participation::Result,
    ) {
        match result {
            settle_community_fund_participation::Result::Committed(_) => {
                self.set_sns_token_swap_lifecycle(Lifecycle::Committed)
            }
            settle_community_fund_participation::Result::Aborted(_) => {
                self.set_sns_token_swap_lifecycle(Lifecycle::Aborted)
            }
        }
    }
}

#[cfg(test)]
mod test_wait_for_quiet {
    use crate::pb::v1::{ProposalData, Tally, WaitForQuietState};
    use ic_nns_common::pb::v1::ProposalId;
    use proptest::prelude::{prop_assert, proptest};

    proptest! {
        /// This test ensures that none of the asserts in
        /// `evaluate_wait_for_quiet` fire, and that the wait-for-quiet
        /// deadline is only ever increased, if at all.
        #[test]
        fn test_evaluate_wait_for_quiet(voting_period_seconds in 3600u64..604_800,
                                        now_seconds in 0u64..1_000_000,
                                        old_yes in 0u64..1_000_000,
                                        old_no in 0u64..1_000_000,
                                        old_total in 10_000_000u64..100_000_000,
                                        yes_votes in 0u64..1_000_000,
                                        no_votes in 0u64..1_000_000,
    ) {
            let current_deadline_timestamp_seconds = voting_period_seconds;
            let proposal_timestamp_seconds = 0; // initial timestamp is always 0
            let mut proposal = ProposalData {
                id: Some(ProposalId { id: 0 }),
                proposal_timestamp_seconds,
                wait_for_quiet_state: Some(WaitForQuietState {
                    current_deadline_timestamp_seconds,
                }),
                ..ProposalData::default()
            };
            let old_tally = Tally {
                timestamp_seconds: now_seconds,
                yes: old_yes,
                no: old_no,
                total: old_total,
            };
            let new_tally = Tally {
                timestamp_seconds: now_seconds,
                yes: old_yes + yes_votes,
                no: old_no + no_votes,
                total: old_total,
            };
            proposal.evaluate_wait_for_quiet(
                now_seconds,
                voting_period_seconds,
                &old_tally,
                &new_tally,
            );
            let new_deadline = proposal
                .wait_for_quiet_state
                .unwrap()
                .current_deadline_timestamp_seconds;
            prop_assert!(new_deadline >= current_deadline_timestamp_seconds);
        }
        }
}

impl ProposalStatus {
    /// Return true if this status is 'final' in the sense that no
    /// further state transitions are possible.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            ProposalStatus::Rejected | ProposalStatus::Executed | ProposalStatus::Failed
        )
    }
}

impl ProposalRewardStatus {
    /// Return true if this reward status is 'final' in the sense that
    /// no further state transitions are possible.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            ProposalRewardStatus::Settled | ProposalRewardStatus::Ineligible
        )
    }
}

impl Topic {
    /// When voting rewards are distributed, the voting power of
    /// neurons voting on proposals are weighted by this amount. The
    /// weights are designed to encourage active participation from
    /// neuron holders.
    fn reward_weight(&self) -> f64 {
        match self {
            // There are several (typically over 100) exchange rate
            // proposals per day.
            Topic::ExchangeRate => 0.01,
            // Other topics are unit weighted. Typically a handful of
            // proposals per day (excluding weekends).
            _ => 1.0,
        }
    }
}

impl Tally {
    /// Returns true if this tally corresponds to an adopted proposal.
    ///
    /// A proposal is adopted if and only if the voting power for `yes`
    /// is strictly greater than 1/2 of the total voting power -- counting
    /// neurons that are eligible to vote, but did not.
    fn is_absolute_majority_for_yes(&self) -> bool {
        self.yes > self.total - self.yes
    }
}

impl GovernanceProto {
    /// From the `neurons` part of this `Governance` struct, build the
    /// index (per topic) from followee to set of followers. The
    /// neurons themselves map followers (the neuron ID) to a set of
    /// followees (per topic).
    pub fn build_topic_followee_index(&self) -> BTreeMap<Topic, BTreeMap<u64, BTreeSet<u64>>> {
        let mut topic_followee_index = BTreeMap::new();
        for neuron in self.neurons.values() {
            GovernanceProto::add_neuron_to_topic_followee_index(&mut topic_followee_index, neuron);
        }
        topic_followee_index
    }

    pub fn add_neuron_to_topic_followee_index(
        index: &mut BTreeMap<Topic, BTreeMap<u64, BTreeSet<u64>>>,
        neuron: &Neuron,
    ) {
        for (itopic, followees) in neuron.followees.iter() {
            // Note: if there are topics in the data (e.g.,
            // file) that the Governance struct was
            // (re-)constructed from that are no longer
            // defined in the `enum Topic`, the entries are
            // not put into the topic_followee_index.
            //
            // This is okay, as the topics are only changed on
            // upgrades, and the index is rebuilt on upgrade.
            if let Some(topic) = Topic::from_i32(*itopic) {
                let followee_index = index.entry(topic).or_insert_with(BTreeMap::new);
                for followee in followees.followees.iter() {
                    followee_index
                        .entry(followee.id)
                        .or_insert_with(BTreeSet::new)
                        .insert(neuron.id.as_ref().expect("Neuron must have an id").id);
                }
            }
        }
    }

    pub fn remove_neuron_from_topic_followee_index(
        index: &mut BTreeMap<Topic, BTreeMap<u64, BTreeSet<u64>>>,
        neuron: &Neuron,
    ) {
        for (itopic, followees) in neuron.followees.iter() {
            if let Some(topic) = Topic::from_i32(*itopic) {
                if let Some(followee_index) = index.get_mut(&topic) {
                    for followee in followees.followees.iter() {
                        if let Some(followee_set) = followee_index.get_mut(&followee.id) {
                            followee_set
                                .remove(&neuron.id.as_ref().expect("Neuron must have an id").id);
                            if followee_set.is_empty() {
                                followee_index.remove(&followee.id);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Update `index` to map all the given Neuron's hot keys and controller to
    /// `neuron_id`
    pub fn add_neuron_to_principal_to_neuron_ids_index(
        index: &mut BTreeMap<PrincipalId, HashSet<u64>>,
        neuron_id: u64,
        neuron: &Neuron,
    ) {
        let principals = neuron.hot_keys.iter().chain(neuron.controller.iter());

        for principal in principals {
            Self::add_neuron_to_principal_in_principal_to_neuron_ids_index(
                index, neuron_id, principal,
            );
        }
    }

    pub fn add_neuron_to_principal_in_principal_to_neuron_ids_index(
        index: &mut BTreeMap<PrincipalId, HashSet<u64>>,
        neuron_id: u64,
        principal: &PrincipalId,
    ) {
        let neuron_ids = index.entry(*principal).or_insert_with(HashSet::new);
        neuron_ids.insert(neuron_id);
    }

    /// Update `index` to remove the neuron from the list of neurons mapped to
    /// principals.
    pub fn remove_neuron_from_principal_to_neuron_ids_index(
        index: &mut BTreeMap<PrincipalId, HashSet<u64>>,
        neuron: &Neuron,
    ) {
        let principals = neuron.hot_keys.iter().chain(neuron.controller.iter());

        for principal in principals {
            Self::remove_neuron_from_principal_in_principal_to_neuron_ids_index(
                index, neuron, principal,
            );
        }
    }

    pub fn remove_neuron_from_principal_in_principal_to_neuron_ids_index(
        index: &mut BTreeMap<PrincipalId, HashSet<u64>>,
        neuron: &Neuron,
        principal: &PrincipalId,
    ) {
        let neuron_id = match neuron.id.as_ref() {
            Some(id) => id.id,
            None => return,
        };

        let neuron_ids = index.get_mut(principal);
        // Shouldn't fail if the index is broken, so just continue.
        if neuron_ids.is_none() {
            return;
        }
        let neuron_ids = neuron_ids.unwrap();
        neuron_ids.retain(|nid| *nid != neuron_id);
        // If there are no neurons left, remove the entry from the index.
        if neuron_ids.is_empty() {
            index.remove(principal);
        }
    }

    pub fn build_principal_to_neuron_ids_index(&self) -> BTreeMap<PrincipalId, HashSet<u64>> {
        let mut index = BTreeMap::new();

        for (id, neuron) in self.neurons.iter() {
            Self::add_neuron_to_principal_to_neuron_ids_index(&mut index, *id, neuron);
        }

        index
    }

    pub fn build_known_neuron_name_index(&self) -> HashSet<String> {
        self.neurons
            .iter()
            .filter(|(_id, neuron)| neuron.known_neuron_data.is_some())
            .map(|(_id, neuron)| neuron.known_neuron_data.as_ref().unwrap().name.clone())
            .collect()
    }

    // Returns whether the proposed default following is valid by making
    // sure that the refered to neurons exist.
    fn validate_default_followees(
        &self,
        proposed: &HashMap<i32, Followees>,
    ) -> Result<(), GovernanceError> {
        for followees in proposed.values() {
            for followee in &followees.followees {
                if !self.neurons.contains_key(&followee.id) {
                    return Err(GovernanceError::new_with_message(
                        ErrorType::NotFound,
                        "One or more of the neurons proposed to become\
                         the new default followees don't exist.",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Iterate over all neurons and compute `GovernanceCachedMetrics`
    pub fn compute_cached_metrics(&self, now: u64, icp_supply: Tokens) -> GovernanceCachedMetrics {
        let mut metrics = GovernanceCachedMetrics {
            timestamp_seconds: now,
            total_supply_icp: icp_supply.get_tokens(),
            ..Default::default()
        };

        let minimum_stake_e8s = if let Some(economics) = self.economics.as_ref() {
            economics.neuron_minimum_stake_e8s
        } else {
            0
        };

        for (_, neuron) in self.neurons.iter() {
            metrics.total_staked_e8s += neuron.minted_stake_e8s();

            if neuron.joined_community_fund_timestamp_seconds.unwrap_or(0) > 0 {
                metrics.community_fund_total_staked_e8s += neuron.minted_stake_e8s();
                metrics.community_fund_total_maturity_e8s_equivalent +=
                    neuron.maturity_e8s_equivalent;
            }

            if neuron.cached_neuron_stake_e8s < DEFAULT_TRANSFER_FEE.get_e8s() {
                metrics.garbage_collectable_neurons_count += 1;
            }
            if 0 < neuron.cached_neuron_stake_e8s
                && neuron.cached_neuron_stake_e8s < minimum_stake_e8s
            {
                metrics.neurons_with_invalid_stake_count += 1;
            }

            let dissolve_delay_seconds = neuron.dissolve_delay_seconds(now);

            if dissolve_delay_seconds < 6 * ONE_MONTH_SECONDS {
                metrics.neurons_with_less_than_6_months_dissolve_delay_count += 1;
                metrics.neurons_with_less_than_6_months_dissolve_delay_e8s +=
                    neuron.minted_stake_e8s();
            }

            match neuron.state(now) {
                NeuronState::Unspecified => (),
                NeuronState::Spawning => (),
                NeuronState::Dissolved => {
                    metrics.dissolved_neurons_count += 1;
                    metrics.dissolved_neurons_e8s += neuron.cached_neuron_stake_e8s;
                }
                NeuronState::Dissolving => {
                    metrics.dissolving_neurons_count += 1;
                    let bucket = dissolve_delay_seconds / ONE_YEAR_SECONDS;

                    let e8s_entry = metrics
                        .dissolving_neurons_e8s_buckets
                        .entry(bucket)
                        .or_insert(0.0);
                    *e8s_entry += neuron.minted_stake_e8s() as f64;

                    let count_entry = metrics
                        .dissolving_neurons_count_buckets
                        .entry(bucket)
                        .or_insert(0);
                    *count_entry += 1;
                }
                NeuronState::NotDissolving => {
                    metrics.not_dissolving_neurons_count += 1;
                    let bucket = dissolve_delay_seconds / ONE_YEAR_SECONDS;

                    let e8s_entry = metrics
                        .not_dissolving_neurons_e8s_buckets
                        .entry(bucket)
                        .or_insert(0.0);
                    *e8s_entry += neuron.minted_stake_e8s() as f64;

                    let count_entry = metrics
                        .not_dissolving_neurons_count_buckets
                        .entry(bucket)
                        .or_insert(0);
                    *count_entry += 1;
                }
            }
        }

        // Compute total amount of locked ICP.
        metrics.total_locked_e8s = metrics
            .total_staked_e8s
            .saturating_sub(metrics.dissolved_neurons_e8s);

        metrics
    }
}

/// Summarizes a RewardEvent. Suitable for logging, because the string is
/// bounded in size.
impl fmt::Display for RewardEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RewardEvent {{ day_after_genesis: {} distributed_e8s_equivalent: {}\
                   actual_timestamp_seconds: {} settled_proposals: <vec of size {}> }})",
            self.day_after_genesis,
            self.distributed_e8s_equivalent,
            self.actual_timestamp_seconds,
            self.settled_proposals.len()
        )
    }
}

/// A trait defining common patterns for accessing the CMC canister.
#[async_trait]
pub trait CMC: Send + Sync {
    /// Returns the current neuron maturity modulation.
    async fn neuron_maturity_modulation(&mut self) -> Result<i32, String>;
}

/// A general trait for the environment in which governance is running.
#[async_trait]
pub trait Environment: Send + Sync {
    /// Returns the current time, in seconds since the epoch.
    fn now(&self) -> u64;

    fn set_time_warp(&mut self, _new_time_warp: TimeWarp) {
        panic!("Not implemented.");
    }

    /// Returns a random number.
    ///
    /// This number is the same in all replicas.
    fn random_u64(&mut self) -> u64;

    /// Returns a random byte array with 32 bytes.
    ///
    /// This number is the same in all replicas.
    fn random_byte_array(&mut self) -> [u8; 32];

    /// Executes a `ExecuteNnsFunction`. The standard implementation is
    /// expected to call out to another canister and eventually report the
    /// result back
    ///
    /// See also call_candid_method.
    fn execute_nns_function(
        &self,
        proposal_id: u64,
        update: &ExecuteNnsFunction,
    ) -> Result<(), GovernanceError>;

    /// Returns rough information as to how much the heap can grow.
    ///
    /// The intended use case is for the governance canister to avoid
    /// non-essential memory-consuming operations when the potential for heap
    /// growth becomes limited.
    fn heap_growth_potential(&self) -> HeapGrowthPotential;

    /// Basically, the same as dfn_core::api::call.
    async fn call_canister_method(
        &mut self,
        target: CanisterId,
        method_name: &str,
        request: Vec<u8>,
    ) -> Result<Vec<u8>, (Option<i32>, String)>;
}

/// Rough buckets for how much the heap can still grow.
pub enum HeapGrowthPotential {
    /// The heap can grow without issue.
    NoIssue,

    /// The heap can still grow, but not by much.
    LimitedAvailability,
}

/// A single ongoing update for a single neuron.
/// Releases the lock when destroyed.
struct LedgerUpdateLock {
    nid: u64,
    gov: *mut Governance,
    // Retain this lock even on drop.
    retain: bool,
}

impl Drop for LedgerUpdateLock {
    fn drop(&mut self) {
        if self.retain {
            return;
        }
        // It's always ok to dereference the governance when a LedgerUpdateLock
        // goes out of scope. Indeed, in the scope of any Governance method,
        // &self always remains alive. The 'mut' is not an issue, because
        // 'unlock_neuron' will verify that the lock exists.
        let gov: &mut Governance = unsafe { &mut *self.gov };
        gov.unlock_neuron(self.nid);
    }
}

impl LedgerUpdateLock {
    fn retain(&mut self) {
        self.retain = true;
    }
}

/// The `Governance` canister implements the full public interface of the
/// IC's governance system.
pub struct Governance {
    /// The Governance Protobuf which contains all persistent state of
    /// the IC's governance system. Needs to be stored and retrieved
    /// on upgrades.
    pub proto: GovernanceProto,

    /// Implementation of Environment to make unit testing easier.
    pub env: Box<dyn Environment>,

    /// Implementation of the interface with the Ledger canister.
    ledger: Box<dyn IcpLedger>,

    /// Implementation of the interface with the CMC canister.
    cmc: Box<dyn CMC>,

    /// Cached data structure that (for each topic) maps a followee to
    /// the set of followers. This is the inverse of the mapping from
    /// neuron (follower) to followees, in the neurons. This is a
    /// cached index and will be removed and recreated when the state
    /// is saved and restored.
    ///
    /// Topic -> (neuron ID of followee) -> set of followers.
    pub topic_followee_index: BTreeMap<Topic, BTreeMap<u64, BTreeSet<u64>>>,

    /// Maps Principals to the Neuron IDs of all Neurons that have this
    /// Principal as their controller or as one of their hot keys
    ///
    /// This is a cached index and will be removed and recreated when the state
    /// is saved and restored.
    pub principal_to_neuron_ids_index: BTreeMap<PrincipalId, HashSet<u64>>,

    /// Set of all names given to Known Neurons, to prevent duplication.
    ///
    /// This set is cached and will be removed and recreated when the state is saved and restored.
    pub known_neuron_name_set: HashSet<String>,

    /// Timestamp, in seconds since the unix epoch, until which no proposal
    /// needs to be processed.
    closest_proposal_deadline_timestamp_seconds: u64,

    /// The time of the latest "garbage collection" - when obsolete
    /// proposals were cleaned up.
    pub latest_gc_timestamp_seconds: u64,

    /// The number of proposals after the last time GC was run.
    pub latest_gc_num_proposals: usize,
}

pub fn governance_minting_account() -> AccountIdentifier {
    AccountIdentifier::new(GOVERNANCE_CANISTER_ID.get(), None)
}

pub fn neuron_subaccount(subaccount: Subaccount) -> AccountIdentifier {
    AccountIdentifier::new(GOVERNANCE_CANISTER_ID.get(), Some(subaccount))
}

impl Governance {
    pub fn new(
        mut proto: GovernanceProto,
        env: Box<dyn Environment>,
        ledger: Box<dyn IcpLedger>,
        cmc: Box<dyn CMC>,
    ) -> Self {
        if proto.genesis_timestamp_seconds == 0 {
            proto.genesis_timestamp_seconds = env.now();
        }
        if proto.latest_reward_event.is_none() {
            // Introduce a dummy reward event to mark the origin of the IC era.
            // This is required to be able to compute accurately the rewards for the
            // very first reward distribution.
            proto.latest_reward_event = Some(RewardEvent {
                actual_timestamp_seconds: env.now(),
                day_after_genesis: 0,
                settled_proposals: vec![],
                distributed_e8s_equivalent: 0,
            })
        }

        let mut gov = Self {
            proto,
            env,
            ledger,
            cmc,
            topic_followee_index: BTreeMap::new(),
            principal_to_neuron_ids_index: BTreeMap::new(),
            known_neuron_name_set: HashSet::new(),
            closest_proposal_deadline_timestamp_seconds: 0,
            latest_gc_timestamp_seconds: 0,
            latest_gc_num_proposals: 0,
        };

        gov.initialize_indices();

        gov
    }

    /// Validates that the underlying protobuf is well formed.
    pub fn validate(&self) -> Result<(), GovernanceError> {
        if self.proto.economics.is_none() {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotFound,
                "Network economics was not found",
            ));
        }

        // Make sure that subaccounts are not repeated across neurons.
        let mut subaccounts = HashSet::new();
        for n in self.proto.neurons.values() {
            // For now expect that neurons have pre-assigned ids, since
            // we add them only at genesis.
            let _ =
                n.id.as_ref()
                    .expect("Currently neurons must have been pre-assigned an id.");
            let subaccount = Subaccount(n.account.clone().as_slice().try_into().map_err(|_| {
                GovernanceError::new_with_message(
                    ErrorType::PreconditionFailed,
                    "Invalid subaccount",
                )
            })?);
            if !subaccounts.insert(subaccount) {
                return Err(GovernanceError::new_with_message(
                    ErrorType::PreconditionFailed,
                    "There are two neurons with the same subaccount",
                ));
            }
        }

        self.proto
            .validate_default_followees(&self.proto.default_followees)?;

        Ok(())
    }

    /// Initializes the indices.
    /// Must be called after the state has been externally changed (e.g. by
    /// setting a new proto).
    fn initialize_indices(&mut self) {
        self.topic_followee_index = self.proto.build_topic_followee_index();
        self.principal_to_neuron_ids_index = self.proto.build_principal_to_neuron_ids_index();
        self.known_neuron_name_set = self.proto.build_known_neuron_name_index();
    }

    fn transaction_fee(&self) -> u64 {
        self.economics().transaction_fee_e8s
    }

    /// Generates a new, unused, NeuronId.
    fn new_neuron_id(&mut self) -> NeuronId {
        let mut id = self.env.random_u64();
        // Don't allow IDs that are already in use. In addition, zero
        // is an invalid ID as it can be confused with an unset ID.
        while self.proto.neurons.contains_key(&id) || id == 0 {
            id = self.env.random_u64();
        }
        NeuronId { id }
    }

    fn neuron_not_found_error(nid: &NeuronId) -> GovernanceError {
        GovernanceError::new_with_message(
            ErrorType::NotFound,
            format!("Neuron not found: {:?}", nid),
        )
    }

    fn no_neuron_for_subaccount_error(subaccount: &[u8]) -> GovernanceError {
        GovernanceError::new_with_message(
            ErrorType::NotFound,
            format!("No neuron found for subaccount {:?}", subaccount),
        )
    }

    fn bytes_to_subaccount(bytes: &[u8]) -> Result<icp_ledger::Subaccount, GovernanceError> {
        bytes.try_into().map_err(|_| {
            GovernanceError::new_with_message(ErrorType::PreconditionFailed, "Invalid subaccount")
        })
    }

    pub fn get_neuron(&self, nid: &NeuronId) -> Result<&Neuron, GovernanceError> {
        self.proto
            .neurons
            .get(&nid.id)
            .ok_or_else(|| Self::neuron_not_found_error(nid))
    }

    pub fn get_neuron_mut(&mut self, nid: &NeuronId) -> Result<&mut Neuron, GovernanceError> {
        self.proto
            .neurons
            .get_mut(&nid.id)
            .ok_or_else(|| Self::neuron_not_found_error(nid))
    }

    fn find_neuron(&self, find_by: &NeuronIdOrSubaccount) -> Result<&Neuron, GovernanceError> {
        match find_by {
            NeuronIdOrSubaccount::NeuronId(nid) => self.get_neuron(nid),
            NeuronIdOrSubaccount::Subaccount(sid) => self
                .get_neuron_by_subaccount(&Self::bytes_to_subaccount(sid)?)
                .ok_or_else(|| Self::no_neuron_for_subaccount_error(sid)),
        }
    }

    /// Locks a given neuron for a specific, signaling there is an ongoing
    /// ledger update.
    ///
    /// This stores the in-flight operation in the proto so that, if anything
    /// goes wrong we can:
    ///
    /// 1 - Know what was happening.
    /// 2 - Reconcile the state post-upgrade, if necessary.
    ///
    /// No concurrent updates to this neuron's state are possible
    /// until the lock is released.
    ///
    /// ***** IMPORTANT *****
    /// The return value MUST be allocated to a variable with a name that is NOT
    /// "_" !
    ///
    /// The LedgerUpdateLock must remain alive for the entire duration of the
    /// ledger call. Quoting
    /// https://doc.rust-lang.org/book/ch18-03-pattern-syntax.html#ignoring-an-unused-variable-by-starting-its-name-with-_
    ///
    /// > Note that there is a subtle difference between using only _ and using
    /// > a name that starts with an underscore. The syntax _x still binds
    /// > the value to the variable, whereas _ doesn’t bind at all.
    ///
    /// What this means is that the expression
    /// ```text
    /// let _ = lock_neuron_for_command(...);
    /// ```
    /// is useless, because the
    /// LedgerUpdateLock is a temporary object. It is constructed (and the lock
    /// is acquired), the immediately dropped (and the lock is released).
    ///
    /// However, the expression
    /// ```text
    /// let _my_lock = lock_neuron_for_command(...);
    /// ```
    /// will retain the lock for the entire scope.
    fn lock_neuron_for_command(
        &mut self,
        id: u64,
        command: NeuronInFlightCommand,
    ) -> Result<LedgerUpdateLock, GovernanceError> {
        if self.proto.in_flight_commands.contains_key(&id) {
            return Err(GovernanceError::new_with_message(
                ErrorType::LedgerUpdateOngoing,
                "Neuron has an ongoing ledger udpate.",
            ));
        }

        self.proto.in_flight_commands.insert(id, command);

        Ok(LedgerUpdateLock {
            nid: id,
            gov: self,
            retain: false,
        })
    }

    /// Unlocks a given neuron.
    fn unlock_neuron(&mut self, id: u64) {
        match self.proto.in_flight_commands.remove(&id) {
            None => {
                println!(
                    "Unexpected condition when unlocking neuron {}: the neuron was not registred as 'in flight'",
                    id
                );
            }
            // This is the expected case...
            Some(_) => (),
        }
    }

    /// Add a neuron to the list of neurons and update
    /// `principal_to_neuron_ids_index`
    ///
    /// Fails under the following conditions:
    /// - the maximum number of neurons has been reached, or
    /// - the given `neuron_id` already exists in `self.proto.neurons`, or
    /// - the neuron's controller `PrincipalId` is not self-authenticating.
    fn add_neuron(&mut self, neuron_id: u64, neuron: Neuron) -> Result<(), GovernanceError> {
        if neuron_id == 0 {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Cannot add neuron with ID zero".to_string(),
            ));
        }
        {
            let neuron_real_id = neuron.id.as_ref().map(|x| x.id).unwrap_or(0);
            if neuron_real_id != neuron_id {
                return Err(GovernanceError::new_with_message(
                    ErrorType::PreconditionFailed,
                    format!(
                        "The neuron's ID {} does not match the provided ID {}",
                        neuron_real_id, neuron_id
                    ),
                ));
            }
        }

        // New neurons are not allowed when the heap is too large.
        self.check_heap_can_grow()?;

        if self.proto.neurons.len() + 1 > MAX_NUMBER_OF_NEURONS {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Cannot add neuron. Max number of neurons reached.",
            ));
        }
        if self.proto.neurons.contains_key(&neuron_id) {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!(
                    "Cannot add neuron. There is already a neuron with id: {:?}",
                    neuron_id
                ),
            ));
        }

        if !neuron.controller.unwrap().is_self_authenticating() {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Cannot add neuron, controller PrincipalId must be self-authenticating".to_string(),
            ));
        }

        GovernanceProto::add_neuron_to_principal_to_neuron_ids_index(
            &mut self.principal_to_neuron_ids_index,
            neuron_id,
            &neuron,
        );

        GovernanceProto::add_neuron_to_topic_followee_index(
            &mut self.topic_followee_index,
            &neuron,
        );

        self.proto.neurons.insert(neuron_id, neuron);

        Ok(())
    }

    /// Remove a neuron from the list of neurons and update
    /// `principal_to_neuron_ids_index`
    ///
    /// Fail if the given `neuron_id` doesn't exist in `self.proto.neurons`
    fn remove_neuron(&mut self, neuron_id: u64, neuron: Neuron) -> Result<(), GovernanceError> {
        if !self.proto.neurons.contains_key(&neuron_id) {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotFound,
                format!(
                    "Cannot remove neuron. Can't find a neuron with id: {:?}",
                    neuron_id
                ),
            ));
        }

        GovernanceProto::remove_neuron_from_principal_to_neuron_ids_index(
            &mut self.principal_to_neuron_ids_index,
            &neuron,
        );

        GovernanceProto::remove_neuron_from_topic_followee_index(
            &mut self.topic_followee_index,
            &neuron,
        );

        self.proto.neurons.remove(&neuron_id);

        Ok(())
    }

    /// Return the Neuron IDs of all Neurons that have `principal` as their
    /// controller or as one of their hot keys.
    pub fn get_neuron_ids_by_principal(&self, principal: &PrincipalId) -> Vec<u64> {
        self.principal_to_neuron_ids_index
            .get(principal)
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Return the union of `followees` with the set of Neuron IDs of all
    /// Neurons that directly follow the `followees` w.r.t. the
    /// topic `NeuronManagement`.
    pub fn get_managed_neuron_ids_for(&self, followees: &[u64]) -> Vec<u64> {
        // Tap into the `topic_followee_index` for followers of level zero neurons.
        let mut managed: HashSet<u64> = followees.iter().copied().collect();
        for followee in followees {
            if let Some(followers) = self
                .topic_followee_index
                .get(&Topic::NeuronManagement)
                .and_then(|m| m.get(followee))
            {
                managed.extend(followers)
            }
        }

        managed.iter().copied().collect()
    }

    /// See `ListNeurons`.
    pub fn list_neurons_by_principal(
        &self,
        req: &ListNeurons,
        caller: &PrincipalId,
    ) -> ListNeuronsResponse {
        let now = self.env.now();
        let implicitly_requested_neurons = if req.include_neurons_readable_by_caller {
            self.get_neuron_ids_by_principal(caller)
        } else {
            Vec::new()
        };
        let requested_list = || {
            req.neuron_ids
                .iter()
                .chain(implicitly_requested_neurons.iter())
        };
        ListNeuronsResponse {
            neuron_infos: requested_list()
                .filter_map(|x| {
                    self.proto
                        .neurons
                        .get(x)
                        .map(|y| (*x, y.get_neuron_info(now)))
                })
                .collect(),
            full_neurons: requested_list()
                .filter_map(|x| self.get_full_neuron(&NeuronId { id: *x }, caller).ok())
                .collect(),
        }
    }

    /// Returns a neuron, given a subaccount.
    ///
    /// Currently we just do linear search on the neurons. We tried an index at
    /// some point, but the index was too big, took too long to build and
    /// ultimately lowered our max possible number of neurons, so we
    /// "downgraded" to linear search.
    ///
    /// Consider changing this if getting a neuron by subaccount ever gets in a
    /// hot path.
    pub fn get_neuron_by_subaccount(&self, subaccount: &Subaccount) -> Option<&Neuron> {
        self.proto.neurons.values().find(|&n| {
            if let Ok(s) = &&Subaccount::try_from(&n.account[..]) {
                return s == subaccount;
            }
            false
        })
    }

    pub fn get_neuron_by_subaccount_mut(&mut self, subaccount: &Subaccount) -> Option<&mut Neuron> {
        self.proto.neurons.values_mut().find(|n| {
            if let Ok(s) = &&Subaccount::try_from(&n.account[..]) {
                return s == subaccount;
            }
            false
        })
    }

    /// Returns a list of known neurons, neurons that have been given a name.
    pub fn list_known_neurons(&self) -> ListKnownNeuronsResponse {
        let known_neurons: Vec<KnownNeuron> = self
            .proto
            .neurons
            .iter()
            .filter(|(_id, neuron)| neuron.known_neuron_data.is_some())
            .map(|(id, neuron)| KnownNeuron {
                id: Some(NeuronId { id: *id }),
                known_neuron_data: neuron.known_neuron_data.clone(),
            })
            .collect();
        ListKnownNeuronsResponse { known_neurons }
    }

    /// Claim the neurons supplied by the GTC on behalf of `new_controller`
    ///
    /// For each neuron ID in `neuron_ids`, check that the corresponding neuron
    /// exists in `self.proto.neurons` and the neuron's controller is the GTC.
    /// If the neuron is in the expected state, set the neuron's controller to
    /// `new_controller` and set other fields (e.g.
    /// `created_timestamp_seconds`).
    pub fn claim_gtc_neurons(
        &mut self,
        caller: &PrincipalId,
        new_controller: PrincipalId,
        neuron_ids: Vec<NeuronId>,
    ) -> Result<(), GovernanceError> {
        if caller != GENESIS_TOKEN_CANISTER_ID.get_ref() {
            return Err(GovernanceError::new(ErrorType::NotAuthorized));
        }

        let ids_are_valid = neuron_ids.iter().all(|id| {
            if let Some(neuron) = self.proto.neurons.get(&id.id) {
                neuron.controller.as_ref() == Some(GENESIS_TOKEN_CANISTER_ID.get_ref())
            } else {
                false
            }
        });

        if !ids_are_valid {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "At least one supplied NeuronId either does not have an associated Neuron \
                or the associated Neuron is not controlled by the GTC",
            ));
        }

        for neuron_id in neuron_ids {
            let neuron = self.proto.neurons.get_mut(&neuron_id.id).unwrap();
            let old_controller = neuron.controller.expect("Neuron must have a controller");
            neuron.controller = Some(new_controller);
            neuron.created_timestamp_seconds = self.env.now();
            GovernanceProto::remove_neuron_from_principal_in_principal_to_neuron_ids_index(
                &mut self.principal_to_neuron_ids_index,
                neuron,
                &old_controller,
            );

            GovernanceProto::add_neuron_to_principal_in_principal_to_neuron_ids_index(
                &mut self.principal_to_neuron_ids_index,
                neuron_id.id,
                &new_controller,
            );
        }

        Ok(())
    }

    /// Transfer a GTC neuron to a recipient neuron.
    ///
    /// This will transfer the stake of the donor neuron to the recipient
    /// neuron, and perform a ledger transfer from the donor neuron's
    /// sub-account to the recipient neuron's sub-account. The donor neuron
    /// will then be deleted.
    pub async fn transfer_gtc_neuron(
        &mut self,
        caller: &PrincipalId,
        donor_neuron_id: &NeuronId,
        recipient_neuron_id: &NeuronId,
    ) -> Result<(), GovernanceError> {
        if caller != GENESIS_TOKEN_CANISTER_ID.get_ref() {
            return Err(GovernanceError::new(ErrorType::NotAuthorized));
        }

        let donor_neuron = self.get_neuron(donor_neuron_id)?;
        let recipient_neuron = self.get_neuron(recipient_neuron_id)?;

        if donor_neuron.controller.as_ref() != Some(GENESIS_TOKEN_CANISTER_ID.get_ref()) {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Donor neuron is not controlled by the GTC",
            ));
        }

        let transaction_fee = self.transaction_fee();

        let donor_subaccount = Subaccount::try_from(&donor_neuron.account[..])
            .expect("Couldn't create a Subaccount from donor_neuron");

        let recipient_subaccount = Subaccount::try_from(&recipient_neuron.account[..])
            .expect("Couldn't create a Subaccount from recipient_neuron");

        let recipient_account_identifier = neuron_subaccount(recipient_subaccount);

        let transfer_amount_doms = donor_neuron.cached_neuron_stake_e8s - transaction_fee;

        let _ = self
            .ledger
            .transfer_funds(
                transfer_amount_doms,
                transaction_fee,
                Some(donor_subaccount),
                recipient_account_identifier,
                0,
            )
            .await?;

        let donor_neuron = donor_neuron.clone();
        self.remove_neuron(donor_neuron_id.id, donor_neuron)?;

        let recipient_neuron = self.get_neuron_mut(recipient_neuron_id)?;
        recipient_neuron.cached_neuron_stake_e8s += transfer_amount_doms;
        Ok(())
    }

    /// Disburse the stake of a neuron.
    ///
    /// This causes the stake of a neuron to be disbursed to the provided
    /// principal (and optional subaccount). If `amount` is provided then
    /// that amount is disbursed.
    ///
    /// Note that we don't enforce that 'amount' is actually smaller
    /// than or equal to the cached stake in the neuron.
    /// This will allow a user to still disburse funds if:
    /// - Someone transferred more funds to the neuron's subaccount after the
    ///   the initial neuron claim that we didn't know about.
    /// - The transfer of funds previously failed for some reason (e.g. the
    ///   ledger was unavailable or broken).
    ///
    /// On success returns the block height at which the transfer happened.
    ///
    /// Preconditions:
    /// - The neuron exists.
    /// - The caller is the controller of the the neuron.
    /// - The neuron's state is `Dissolved` at the current timestamp.
    pub async fn disburse_neuron(
        &mut self,
        id: &NeuronId,
        caller: &PrincipalId,
        disburse: &manage_neuron::Disburse,
    ) -> Result<u64, GovernanceError> {
        let transaction_fee_e8s = self.transaction_fee();
        let neuron = self.proto.neurons.get_mut(&id.id).ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::NotFound,
                format!("Neuron not found in governance canister: {}", id.id),
            )
        })?;

        if !neuron.is_controlled_by(caller) {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                format!(
                    "Caller '{:?}' is not authorized to control neuron '{}'.",
                    caller, id.id
                ),
            ));
        }

        let state = neuron.state(self.env.now());
        if state != NeuronState::Dissolved {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!(
                    "Neuron {} has NOT been dissolved. It is in state {:?}",
                    id.id, state
                ),
            ));
        }

        if !neuron.kyc_verified {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!("Neuron {} is not kyc verified.", id.id),
            ));
        }

        let from_subaccount = subaccount_from_slice(&neuron.account)?.ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!(
                    "Neuron {} has no associated subaccount, \
                     therefore we cannot know the corresponding ledger account.",
                    id.id
                ),
            )
        })?;

        // If no account was provided, transfer to the caller's account.
        let to_account: AccountIdentifier = match disburse.to_account.as_ref() {
            None => AccountIdentifier::new(*caller, None),
            Some(ai_pb) => AccountIdentifier::try_from(ai_pb).map_err(|e| {
                GovernanceError::new_with_message(
                    ErrorType::InvalidCommand,
                    format!("The recipient's subaccount is invalid due to: {}", e),
                )
            })?,
        };

        let fees_amount_e8s = neuron.neuron_fees_e8s;
        // Calculate the amount to transfer, and adjust the cached stake,
        // accordingly. Make sure no matter what the user disburses we still
        // take the fees into account.
        //
        // Note that the implementation of minted_stake_e8s() is effectively:
        //   neuron.cached_neuron_stake_e8s.saturating_sub(neuron.neuron_fees_e8s)
        // So there is symmetry here in that we are subtracting
        // fees_amount_e8s from both sides of this `map_or`.
        let mut disburse_amount_e8s = disburse
            .amount
            .as_ref()
            .map_or(neuron.minted_stake_e8s(), |a| {
                a.e8s.saturating_sub(fees_amount_e8s)
            });

        // Subtract the transaction fee from the amount to disburse since it'll
        // be deducted from the source (the neuron's) account.
        if disburse_amount_e8s > transaction_fee_e8s {
            disburse_amount_e8s -= transaction_fee_e8s
        }

        // Add the neuron's id to the set of neurons with ongoing ledger updates.
        let now = self.env.now();
        let _neuron_lock = self.lock_neuron_for_command(
            id.id,
            NeuronInFlightCommand {
                timestamp: now,
                command: Some(InFlightCommand::Disburse(disburse.clone())),
            },
        )?;

        // We need to do 2 transfers:
        // 1 - Burn the neuron management fees.
        // 2 - Transfer the the disbursed amount to the target account

        // Transfer 1 - burn the fees, but only if the value exceeds the cost of
        // a transaction fee, as the ledger doesn't support burn transfers for
        // an amount less than the transaction fee.
        if fees_amount_e8s > transaction_fee_e8s {
            let now = self.env.now();
            let _result = self
                .ledger
                .transfer_funds(
                    fees_amount_e8s,
                    0, // Burning transfers don't pay a fee.
                    Some(from_subaccount),
                    governance_minting_account(),
                    now,
                )
                .await?;
        }

        let neuron = self
            .proto
            .neurons
            .get_mut(&id.id)
            .expect("Expected the parent neuron to exist");

        // Update the stake and the fees to reflect the burning above.
        if neuron.cached_neuron_stake_e8s > fees_amount_e8s {
            neuron.cached_neuron_stake_e8s -= fees_amount_e8s;
        } else {
            neuron.cached_neuron_stake_e8s = 0;
        }
        neuron.neuron_fees_e8s = 0;

        // Transfer 2 - Disburse to the chosen account. This may fail if the
        // user told us to disburse more than they had in their account (but
        // the burn still happened).
        let now = self.env.now();
        let block_height = self
            .ledger
            .transfer_funds(
                disburse_amount_e8s,
                transaction_fee_e8s,
                Some(from_subaccount),
                to_account,
                now,
            )
            .await?;

        let neuron = self
            .proto
            .neurons
            .get_mut(&id.id)
            .expect("Expected the parent neuron to exist");

        let to_deduct = disburse_amount_e8s + transaction_fee_e8s;
        // The transfer was successful we can change the stake of the neuron.
        neuron.cached_neuron_stake_e8s = neuron.cached_neuron_stake_e8s.saturating_sub(to_deduct);

        Ok(block_height)
    }

    /// Splits a neuron into two neurons.
    ///
    /// The parent neuron's stake is decreased by the amount specified in
    /// Split, while the child neuron is created with a stake
    /// equal to that amount, minus the transfer fee.
    ///
    /// The child neuron inherits all the properties of its parent
    /// including age and dissolve state.
    ///
    /// On success returns the newly created neuron's id.
    ///
    /// Preconditions:
    /// - The parent neuron exists
    /// - The caller is the controller of the neuron.
    /// - The parent neuron is not already undergoing ledger updates.
    /// - The parent neuron is not spawning.
    /// - The staked amount minus amount to split is more than the minimum
    ///   stake.
    /// - The amount to split minus the transfer fee is more than the minimum
    ///   stake.
    pub async fn split_neuron(
        &mut self,
        id: &NeuronId,
        caller: &PrincipalId,
        split: &manage_neuron::Split,
    ) -> Result<NeuronId, GovernanceError> {
        // New neurons are not allowed when the heap is too large.
        self.check_heap_can_grow()?;

        let min_stake = self
            .proto
            .economics
            .as_ref()
            .expect("Governance must have economics.")
            .neuron_minimum_stake_e8s;

        let transaction_fee_e8s = self.transaction_fee();

        // Get the neuron and clone to appease the borrow checker.
        // We'll get a mutable reference when we need to change it later.
        let parent_neuron = self.get_neuron(id)?.clone();

        if parent_neuron.state(self.env.now()) == NeuronState::Spawning {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Can't perform operation on neuron: Neuron is spawning.",
            ));
        }

        let parent_nid = parent_neuron.id.as_ref().expect("Neurons must have an id");

        if !parent_neuron.is_controlled_by(caller) {
            return Err(GovernanceError::new(ErrorType::NotAuthorized));
        }

        if split.amount_e8s < min_stake + transaction_fee_e8s {
            return Err(GovernanceError::new_with_message(
                ErrorType::InsufficientFunds,
                format!(
                    "Trying to split a neuron with argument {} e8s. This is too little: \
                      at the minimum, one needs the minimum neuron stake, which is {} e8s, \
                      plus the transaction fee, which is {}. Hence the mininum split amount is {}.",
                    split.amount_e8s,
                    min_stake,
                    transaction_fee_e8s,
                    min_stake + transaction_fee_e8s
                ),
            ));
        }

        if parent_neuron.minted_stake_e8s() < min_stake + split.amount_e8s {
            return Err(GovernanceError::new_with_message(
                ErrorType::InsufficientFunds,
                format!(
                    "Trying to split {} e8s out of neuron {}. \
                     This is not allowed, because the parent has stake {} e8s. \
                     If the requested amount was subtracted from it, there would be less than \
                     the minimum allowed stake, which is {} e8s. ",
                    split.amount_e8s,
                    parent_nid.id,
                    parent_neuron.minted_stake_e8s(),
                    min_stake
                ),
            ));
        }

        let creation_timestamp_seconds = self.env.now();
        let child_nid = self.new_neuron_id();

        let from_subaccount = subaccount_from_slice(&parent_neuron.account)?.ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "From subaccount not present.",
            )
        })?;

        let to_subaccount = Subaccount(self.env.random_byte_array());

        // Make sure there isn't already a neuron with the same sub-account.
        if self
            .proto
            .neurons
            .values()
            .any(|n| n.account == to_subaccount.0)
        {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "There is already a neuron with the same subaccount.",
            ));
        }

        let in_flight_command = NeuronInFlightCommand {
            timestamp: creation_timestamp_seconds,
            command: Some(InFlightCommand::Split(split.clone())),
        };

        let staked_amount = split.amount_e8s - transaction_fee_e8s;

        // Make sure the parent neuron is not already undergoing a ledger
        // update.
        let _parent_lock =
            self.lock_neuron_for_command(parent_nid.id, in_flight_command.clone())?;

        // Before we do the transfer, we need to save the neuron in the map
        // otherwise a trap after the transfer is successful but before this
        // method finishes would cause the funds to be lost.
        // However the new neuron is not yet ready to be used as we can't know
        // whether the transfer will succeed, so we temporarily set the
        // stake to 0 and only change it after the transfer is successful.
        let child_neuron = Neuron {
            id: Some(child_nid.clone()),
            account: to_subaccount.to_vec(),
            controller: Some(*caller),
            hot_keys: parent_neuron.hot_keys.clone(),
            cached_neuron_stake_e8s: 0,
            neuron_fees_e8s: 0,
            created_timestamp_seconds: creation_timestamp_seconds,
            aging_since_timestamp_seconds: parent_neuron.aging_since_timestamp_seconds,
            dissolve_state: parent_neuron.dissolve_state.clone(),
            followees: parent_neuron.followees.clone(),
            recent_ballots: Vec::new(),
            kyc_verified: parent_neuron.kyc_verified,
            transfer: None,
            maturity_e8s_equivalent: 0,
            staked_maturity_e8s_equivalent: None,
            auto_stake_maturity: parent_neuron.auto_stake_maturity,
            not_for_profit: parent_neuron.not_for_profit,
            // We allow splitting of a neuron that has joined the
            // community fund: both resulting neurons remain members
            // of the fund with the same "join date".
            joined_community_fund_timestamp_seconds: parent_neuron
                .joined_community_fund_timestamp_seconds,
            known_neuron_data: None,
            spawn_at_timestamp_seconds: None,
        };

        // Add the child neuron to the set of neurons undergoing ledger updates.
        let _child_lock = self.lock_neuron_for_command(child_nid.id, in_flight_command.clone())?;

        // We need to add the "embryo neuron" to the governance proto only after
        // acquiring the lock. Indeed, in case there is already a pending
        // command, we return without state rollback. If we had already created
        // the embryo, it would not be garbage collected.
        self.add_neuron(child_nid.id, child_neuron.clone())?;

        // Do the transfer.

        let now = self.env.now();
        let result: Result<u64, NervousSystemError> = self
            .ledger
            .transfer_funds(
                staked_amount,
                transaction_fee_e8s,
                Some(from_subaccount),
                neuron_subaccount(to_subaccount),
                now,
            )
            .await;

        if let Err(error) = result {
            let error = GovernanceError::from(error);
            // If we've got an error, we assume the transfer didn't happen for
            // some reason. The only state to cleanup is to delete the child
            // neuron, since we haven't mutated the parent yet.
            self.remove_neuron(child_nid.id, child_neuron)?;
            println!(
                "Neuron stake transfer of split_neuron: {:?} \
                     failed with error: {:?}. Neuron can't be staked.",
                child_nid, error
            );
            return Err(error);
        }

        // Get the neuron again, but this time a mutable reference.
        // Expect it to exist, since we acquired a lock above.
        let parent_neuron = self.get_neuron_mut(id).expect("Neuron not found");

        // Update the state of the parent and child neurons.
        parent_neuron.cached_neuron_stake_e8s -= split.amount_e8s;

        let child_neuron = self
            .get_neuron_mut(&child_nid)
            .expect("Expected the child neuron to exist");

        child_neuron.cached_neuron_stake_e8s = staked_amount;
        Ok(child_nid)
    }

    /// Merge one neuron (the "source" provided by the Merge argument) into
    /// another (the "target" specified by the 'id').
    ///
    /// The source neuron's stake, maturity and age are moved into the target.
    /// Any fees the source neuron are burned before the transfer occurs.
    ///
    /// On success the target neuron contains all the stake, maturity and age
    /// of both neurons. The source neuron has 0 stake, 0 maturity and 0 age.
    /// Current fees are not affected in either neuron. The dissolve delay of
    /// the target neuron is the greater of the dissolve delay of the two,
    /// while the source remains unchanged.
    ///
    /// Preconditions:
    /// - Source id and target id cannot be the same
    /// - Target neuron must be owned by the caller
    /// - Source neuron must be owned by the caller
    /// - Source neuron's kyc_verified field must match target
    /// - Source neuron's not_for_profit field must match target
    /// - Source neuron and target neuron have the same ManageNeuron following
    /// - Cannot merge neurons that have been dedicated to the community fund
    /// - Source neuron cannot be dedicated to the community fund
    /// - Target neuron cannot be dedicated to the community fund
    /// - Source neuron cannot be in spawning state
    /// - Target neuron cannot be in spawning state
    /// - Subaccount of source neuron to be merged must be present
    /// - Subaccount of target neuron to be merged must be present
    /// - Neither neuron can be the proposer of an open proposal
    /// - Neither neuron can be the subject of a MergeNeuron proposal
    /// - Source neuron must exist
    /// - Target neuron must exist
    ///
    /// Considerations:
    /// - If the stake of the source neuron is bigger than the transaction fee
    ///   it will be merged into the stake of the target neuron; if it is less
    ///   than the transaction fee, the maturity of the source neuron will
    ///   still be merged into the maturity of the target neuron.
    pub async fn merge_neurons(
        &mut self,
        id: &NeuronId,
        caller: &PrincipalId,
        merge: &manage_neuron::Merge,
    ) -> Result<(), GovernanceError> {
        let source_id = merge.source_neuron_id.as_ref().ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                "There was no source neuron id",
            )
        })?;

        if id.id == source_id.id {
            return Err(GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                "Cannot merge a neuron into itself",
            ));
        }

        // Get the neuron and clone to appease the borrow checker.
        let target_neuron = self.get_neuron(id)?.clone();
        if !target_neuron.is_controlled_by(caller) {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                "Target neuron must be owned by the caller",
            ));
        }

        let source_neuron = self.get_neuron(source_id)?.clone();
        if !source_neuron.is_controlled_by(caller) {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                "Source neuron must be owned by the caller",
            ));
        }

        if source_neuron.state(self.env.now()) == NeuronState::Spawning {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Can't perform operation on neuron: Source neuron is spawning.",
            ));
        }

        if target_neuron.state(self.env.now()) == NeuronState::Spawning {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Can't perform operation on neuron: Target neuron is spawning.",
            ));
        }

        if source_neuron.neuron_managers() != target_neuron.neuron_managers() {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "ManageNeuron following of source and target does not match",
            ));
        }

        if source_neuron.kyc_verified != target_neuron.kyc_verified {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Source neuron's kyc_verified field does not match target",
            ));
        }
        if source_neuron.not_for_profit != target_neuron.not_for_profit {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Source neuron's not_for_profit field does not match target",
            ));
        }
        if source_neuron.is_community_fund_neuron() || target_neuron.is_community_fund_neuron() {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Cannot merge neurons that have been dedicated to the community fund",
            ));
        }

        let from_subaccount = subaccount_from_slice(&source_neuron.account)?.ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                "Subaccount of source neuron is not valid",
            )
        })?;
        let to_subaccount = subaccount_from_slice(&target_neuron.account)?.ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                "Subaccount of target neuron is not valid",
            )
        })?;

        let now = self.env.now();

        let in_flight_command = NeuronInFlightCommand {
            timestamp: now,
            command: Some(InFlightCommand::Merge(merge.clone())),
        };

        // Make sure the source and target neurons are not already
        // undergoing a ledger update.
        let _target_lock = self.lock_neuron_for_command(id.id, in_flight_command.clone())?;
        let _source_lock = self.lock_neuron_for_command(source_id.id, in_flight_command.clone())?;

        // Do not allow this command to be called for any neuron that is the
        // involved in an open proposal.
        fn involved_with_proposal(proto: &GovernanceProto, id: &NeuronId) -> bool {
            proto.proposals.values().any(|p| {
                p.status() == ProposalStatus::Open
                    && (p.proposer.as_ref() == Some(id)
                        || (p.is_manage_neuron()
                            && p.proposal.as_ref().map_or(false, |pr| {
                                pr.managed_neuron()
                                    == Some(NeuronIdOrSubaccount::NeuronId(id.clone()))
                            })))
            })
        }
        if involved_with_proposal(&self.proto, id) || involved_with_proposal(&self.proto, source_id)
        {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Cannot merge neurons that are involved in open proposals",
            ));
        }

        let transaction_fee_e8s = self.transaction_fee();

        let source_neuron_fees_e8s = self.get_neuron(source_id)?.neuron_fees_e8s;

        // Before transferring stake from the source to the target, burn any
        // fees present in the source neuron.
        let mut subtract_fees = false;
        if source_neuron_fees_e8s > transaction_fee_e8s {
            let _result = self
                .ledger
                .transfer_funds(
                    source_neuron_fees_e8s,
                    0, // Burning transfers don't pay a fee.
                    Some(from_subaccount),
                    governance_minting_account(),
                    now,
                )
                .await?;
            subtract_fees = true;
        }

        let source_neuron_mut = self
            .get_neuron_mut(source_id)
            .expect("Expected the source neuron to exist");

        if subtract_fees {
            source_neuron_mut.cached_neuron_stake_e8s = source_neuron_mut
                .cached_neuron_stake_e8s
                .saturating_sub(source_neuron_fees_e8s);

            // It could be that, during the await above, the source_neuron
            // makes a new proposal and thus the fees are increased and if we
            // then just set the fees to 0 here, effectively the source_neuron
            // prevented from paying the fees.
            source_neuron_mut.neuron_fees_e8s = source_neuron_mut
                .neuron_fees_e8s
                .saturating_sub(source_neuron_fees_e8s);
        }

        let source_dissolve_delay = source_neuron.dissolve_delay_seconds(now);
        let source_age_seconds = if source_neuron.is_dissolved(now) {
            // Do not credit age from dissolved neurons.
            0
        } else {
            source_neuron.age_seconds(now)
        };
        let source_stake_e8s = source_neuron_mut.minted_stake_e8s();
        let source_stake_less_transaction_fee_e8s =
            source_stake_e8s.saturating_sub(transaction_fee_e8s);

        if source_stake_less_transaction_fee_e8s > 0 {
            // We must zero out the source neuron's cached stake before
            // submitting the call to transfer_funds. If we do not do this,
            // there would be a window of opportunity -- from the moment the
            // stake is transferred but before the cached stake is updated --
            // when a proposal could be submitted and rejected on behalf of
            // the source neuron (since cached stake is high enough), but that
            // would be impossible to charge because the account had been
            // emptied. To guard against this, we pre-emptively set the stake
            // to zero, and set it back in case of transfer failure.
            //
            // Another important reason to set the cached stake to zero (net
            // fees) is so that the source neuron cannot use the stake that is
            // getting merged to vote or propose. Also, the source neuron
            // should not be able to increase stake while locked because we do
            // not allow the source to have pending proposals.
            source_neuron_mut.cached_neuron_stake_e8s = source_neuron_mut
                .cached_neuron_stake_e8s
                .saturating_sub(source_stake_e8s);

            // Reset source aging. In other words, if it was aging before, it
            // is still aging now, although the timer is reset to the time of
            // the merge -- but only if there is stake being transferred.
            // Since all fees have been burned (if they were greater in value
            // than the transaction fee) and since this neuron is not
            // currently participating in any proposal, it means the cached
            // stake is 0 and increasing the stake will not take advantage of
            // this age. However, it is consistent with the use of
            // aging_since_timestamp_seconds that we simply reset the age
            // here, since we do not change the dissolve state in any other
            // way.
            let source_age_timestamp_seconds = source_neuron_mut.aging_since_timestamp_seconds;
            if source_neuron_mut.aging_since_timestamp_seconds != u64::MAX {
                source_neuron_mut.aging_since_timestamp_seconds = now;
            }

            let _block_height: u64 = self
                .ledger
                .transfer_funds(
                    source_stake_less_transaction_fee_e8s,
                    transaction_fee_e8s,
                    Some(from_subaccount),
                    neuron_subaccount(to_subaccount),
                    now,
                )
                .await
                .map_err(|err| {
                    let source_neuron_mut = self
                        .proto
                        .neurons
                        .get_mut(&source_id.id)
                        .expect("Expected the source neuron to exist");
                    source_neuron_mut.cached_neuron_stake_e8s += source_stake_e8s;
                    source_neuron_mut.aging_since_timestamp_seconds = source_age_timestamp_seconds;
                    err
                })?;
        }

        // Lookup the neuron again, since it may have changed since the
        // (potential) call to the Ledger canister above.
        let source_neuron_mut = self
            .get_neuron_mut(source_id)
            .expect("Expected the source neuron to exist");

        // Set source maturity to zero
        let source_maturity = source_neuron_mut.maturity_e8s_equivalent;
        source_neuron_mut.maturity_e8s_equivalent = 0;
        let source_staked_maturity = source_neuron_mut.staked_maturity_e8s_equivalent;
        source_neuron_mut.staked_maturity_e8s_equivalent = None;

        let mut target_neuron_mut = self
            .get_neuron_mut(id)
            .expect("Expected the target neuron to exist");

        let target_dissolve_delay = target_neuron_mut.dissolve_delay_seconds(now);
        let target_age_seconds = if target_neuron_mut.is_dissolved(now) {
            // Do not credit age from dissolved neurons.
            0
        } else {
            target_neuron_mut.age_seconds(now)
        };
        let highest_dissolve_delay = std::cmp::max(target_dissolve_delay, source_dissolve_delay);
        let target_delta = highest_dissolve_delay.saturating_sub(target_dissolve_delay);

        // Set dissolve delay or when dissolved timestamp of the target to
        // whichever is the greater between the source and target neurons.
        // Note that this must happen before the
        // `aging_since_timestamp_seconds` is updated, because of the various
        // ways in which this call to `increase_dissolve_delay` might change
        // that value. We already know what the aggregate age of the merged
        // neurons should be, so we ignore the changes that this function may
        // make.
        if target_delta > 0 {
            target_neuron_mut.increase_dissolve_delay(now, target_delta.try_into().unwrap())?;
        }

        // Move the source's stake (net fees) and any accumulated
        // neuron age from the source neuron into target.
        let (new_stake_e8s, new_age_seconds) = combine_aged_stakes(
            target_neuron_mut.cached_neuron_stake_e8s,
            target_age_seconds,
            source_stake_less_transaction_fee_e8s,
            source_age_seconds,
        );
        target_neuron_mut.cached_neuron_stake_e8s = new_stake_e8s;
        target_neuron_mut.aging_since_timestamp_seconds = now.saturating_sub(new_age_seconds);

        // Move regular and staked maturity from source neuron to target
        target_neuron_mut.maturity_e8s_equivalent += source_maturity;
        target_neuron_mut.staked_maturity_e8s_equivalent = match (
            target_neuron_mut.staked_maturity_e8s_equivalent,
            source_staked_maturity,
        ) {
            (None, None) => None,
            (Some(x), None) => Some(x),
            (None, Some(y)) => Some(y),
            (Some(x), Some(y)) => Some(x.saturating_add(y)),
        };

        println!(
            "{}Merged neuron {} into {} at {:?}",
            LOG_PREFIX, source_id.id, id.id, now
        );

        Ok(())
    }

    /// Spawn an neuron from an existing neuron's maturity.
    ///
    /// This creates a new neuron and moves some of the existing neuron's maturity
    /// to the new neuron's maturity. The newly created neuron is in spawning state
    /// and the time when it will be spawn is defined according to the NetworkEconomics.
    ///
    /// Pre-conditions:
    /// - The parent neuron exists.
    /// - The caller is the controller of the neuron.
    /// - The controller of the spawned neuron is self-authenticating.
    /// - The parent neuron is not already undergoing ledger updates.
    /// - The parent neuron is not spawning itself.
    /// - The maturity to move to the new neuron must be such that, with every maturity modulation, at least
    ///   NetworkEconomics::neuron_minimum_spawn_stake_e8s are created when the maturity is spawn.
    pub async fn spawn_neuron(
        &mut self,
        id: &NeuronId,
        caller: &PrincipalId,
        spawn: &manage_neuron::Spawn,
    ) -> Result<NeuronId, GovernanceError> {
        // New neurons are not allowed when the heap is too large.
        self.check_heap_can_grow()?;

        let parent_neuron = self.get_neuron(id)?.clone();

        if parent_neuron.state(self.env.now()) == NeuronState::Spawning {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Can't perform operation on neuron: Target neuron is spawning.",
            ));
        }

        if !parent_neuron.is_controlled_by(caller) {
            return Err(GovernanceError::new(ErrorType::NotAuthorized));
        }

        let percentage: u32 = spawn.percentage_to_spawn.unwrap_or(100);
        if percentage > 100 || percentage == 0 {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "The percentage of maturity to spawn must be a value between 1 and 100 (inclusive)."));
        }

        let maturity_to_spawn = parent_neuron
            .maturity_e8s_equivalent
            .checked_mul(percentage as u64)
            .expect("Overflow while processing maturity to spawn.");
        let maturity_to_spawn = maturity_to_spawn.checked_div(100).unwrap();

        // Validate that if a child neuron controller was provided, it is a valid
        // principal.
        let child_controller = if let Some(child_controller_) = &spawn.new_controller {
            child_controller_
        } else {
            parent_neuron
                .controller
                .as_ref()
                .expect("The parent neuron doesn't have a controller.")
        };

        let economics = self
            .proto
            .economics
            .as_ref()
            .expect("Governance does not have NetworkEconomics")
            .clone();

        // Check if the least possible stake this neuron would be spawned with
        // is more than the minimum neuron stake.
        let least_possible_stake = (maturity_to_spawn as f64 * (1f64 - 0.05)) as u64;

        if least_possible_stake < economics.neuron_minimum_stake_e8s {
            return Err(GovernanceError::new_with_message(
                ErrorType::InsufficientFunds,
                "There isn't enough maturity to spawn a new neuron due to worst case maturity modulation.",
            ));
        }

        let child_nid = self.new_neuron_id();

        // use provided sub-account if any, otherwise generate a random one.
        let to_subaccount = match spawn.nonce {
            None => Subaccount(self.env.random_byte_array()),
            Some(nonce_val) => {
                ledger::compute_neuron_staking_subaccount(*child_controller, nonce_val)
            }
        };

        // Make sure there isn't already a neuron with the same sub-account.
        if self
            .proto
            .neurons
            .values()
            .any(|n| n.account == to_subaccount.0)
        {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "There is already a neuron with the same subaccount.",
            ));
        }

        let creation_timestamp_seconds = self.env.now();
        let dissolve_and_spawn_at_timestamp_seconds =
            creation_timestamp_seconds + economics.neuron_spawn_dissolve_delay_seconds;

        let child_neuron = Neuron {
            id: Some(child_nid.clone()),
            account: to_subaccount.to_vec(),
            controller: Some(*child_controller),
            hot_keys: parent_neuron.hot_keys.clone(),
            cached_neuron_stake_e8s: 0,
            neuron_fees_e8s: 0,
            created_timestamp_seconds: creation_timestamp_seconds,
            aging_since_timestamp_seconds: u64::MAX,
            dissolve_state: Some(DissolveState::WhenDissolvedTimestampSeconds(
                dissolve_and_spawn_at_timestamp_seconds,
            )),
            spawn_at_timestamp_seconds: Some(dissolve_and_spawn_at_timestamp_seconds),
            followees: parent_neuron.followees.clone(),
            recent_ballots: Vec::new(),
            kyc_verified: parent_neuron.kyc_verified,
            transfer: None,
            maturity_e8s_equivalent: maturity_to_spawn,
            staked_maturity_e8s_equivalent: None,
            auto_stake_maturity: None,
            not_for_profit: false,
            // We allow spawning of maturity from a neuron that has
            // joined the community fund: the spawned neuron is not
            // considered part of the community fund.
            joined_community_fund_timestamp_seconds: None,
            known_neuron_data: None,
        };

        // `add_neuron` will verify that `child_neuron.controller` `is_self_authenticating()`, so we don't need to check it here.
        self.add_neuron(child_nid.id, child_neuron)?;

        // Get the neurons again, but this time mutable references.
        let parent_neuron = self.get_neuron_mut(id).expect("Neuron not found");

        // Reset the parent's maturity.
        parent_neuron.maturity_e8s_equivalent -= maturity_to_spawn;

        Ok(child_nid)
    }

    pub fn redirect_merge_maturity_to_stake_maturity(
        &mut self,
        id: &NeuronId,
        caller: &PrincipalId,
        merge_maturity: &manage_neuron::MergeMaturity,
    ) -> Result<MergeMaturityResponse, GovernanceError> {
        let stake_maturity = manage_neuron::StakeMaturity {
            percentage_to_stake: Some(merge_maturity.percentage_to_merge),
        };
        let stake_result = self.stake_maturity_of_neuron(id, caller, &stake_maturity);
        match stake_result {
            Ok((_stake_response, merge_response)) => Ok(merge_response),
            Err(e) => Err(e),
        }
    }

    /// Stakes the maturity of a neuron.
    ///
    /// This method allows a neuron controller to stake the currently
    /// existing maturity of a neuron. The caller can choose a percentage
    /// of maturity to merge.
    ///
    /// Pre-conditions:
    /// - The neuron is controlled by `caller`
    /// - The neuron has some maturity to stake.
    /// - The neuron is not in spawning state.
    pub fn stake_maturity_of_neuron(
        &mut self,
        id: &NeuronId,
        caller: &PrincipalId,
        stake_maturity: &manage_neuron::StakeMaturity,
    ) -> Result<(StakeMaturityResponse, MergeMaturityResponse), GovernanceError> {
        let neuron = self.get_neuron(id)?.clone();

        if neuron.state(self.env.now()) == NeuronState::Spawning {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Can't perform operation on neuron: Neuron is spawning.",
            ));
        }

        let nid = neuron.id.as_ref().expect("Neurons must have an id");

        if !neuron.is_controlled_by(caller) {
            return Err(GovernanceError::new(ErrorType::NotAuthorized));
        }

        let percentage_to_stake = stake_maturity.percentage_to_stake.unwrap_or(100);

        if percentage_to_stake > 100 || percentage_to_stake == 0 {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "The percentage of maturity to stake must be a value between 0 (exclusive) and 100 (inclusive)."));
        }

        let mut maturity_to_stake = (neuron
            .maturity_e8s_equivalent
            .saturating_mul(percentage_to_stake as u64))
            / 100;

        if maturity_to_stake > neuron.maturity_e8s_equivalent {
            maturity_to_stake = neuron.maturity_e8s_equivalent;
        }

        let now = self.env.now();
        let in_flight_command = NeuronInFlightCommand {
            timestamp: now,
            command: Some(InFlightCommand::SyncCommand(SyncCommand {})),
        };

        // Lock the neuron so that we're sure that we are not staking the maturity in the middle of another ongoing operation.
        let _neuron_lock = self.lock_neuron_for_command(nid.id, in_flight_command)?;

        // Adjust the maturity of the neuron
        let neuron = self
            .get_neuron_mut(nid)
            .expect("Expected the neuron to exist");

        neuron.maturity_e8s_equivalent = neuron
            .maturity_e8s_equivalent
            .saturating_sub(maturity_to_stake);

        neuron.staked_maturity_e8s_equivalent = Some(
            neuron
                .staked_maturity_e8s_equivalent
                .unwrap_or(0)
                .saturating_add(maturity_to_stake),
        );
        let staked_maturity_e8s = neuron.staked_maturity_e8s_equivalent.unwrap_or(0);
        let new_stake_e8s = neuron.cached_neuron_stake_e8s + staked_maturity_e8s;

        Ok((
            StakeMaturityResponse {
                maturity_e8s: neuron.maturity_e8s_equivalent,
                staked_maturity_e8s,
            },
            MergeMaturityResponse {
                merged_maturity_e8s: maturity_to_stake,
                new_stake_e8s,
            },
        ))
    }

    /// Disburse part of the stake of a neuron into a new neuron, possibly
    /// owned by someone else and with a different dissolve delay.
    ///
    /// The parent neuron's stake is decreased by the amount specified in
    /// DisburseToNeuron, while the child neuron is created with a stake
    /// equal to that amount, minus the transfer fee.
    ///
    /// The child neuron doesn't inherit any of the properties of the parent
    /// neuron, except its following.
    ///
    /// On success returns the newly created neuron's id.
    ///
    /// Preconditions:
    /// - The parent neuron exists
    /// - The caller is the controller of the neuron
    /// - The parent neuron is not already undergoing ledger updates.
    /// - The parent neuron is not in spawning state.
    /// - The parent neuron's state is `Dissolved` at the current timestamp.
    /// - The staked amount minus amount to split is more than the minimum
    ///   stake.
    /// - The amount to split minus the transfer fee is more than the minimum
    ///   stake.
    pub async fn disburse_to_neuron(
        &mut self,
        id: &NeuronId,
        caller: &PrincipalId,
        disburse_to_neuron: &manage_neuron::DisburseToNeuron,
    ) -> Result<NeuronId, GovernanceError> {
        let economics = self
            .proto
            .economics
            .as_ref()
            .expect("Governance must have economics.")
            .clone();

        let creation_timestamp_seconds = self.env.now();
        let transaction_fee_e8s = self.transaction_fee();

        let parent_neuron = self.get_neuron(id)?.clone();
        let parent_nid = parent_neuron.id.as_ref().expect("Neurons must have an id");

        if parent_neuron.state(self.env.now()) == NeuronState::Spawning {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Can't perform operation on neuron: Neuron is spawning.",
            ));
        }

        if !parent_neuron.is_controlled_by(caller) {
            return Err(GovernanceError::new(ErrorType::NotAuthorized));
        }

        let min_stake = economics.neuron_minimum_stake_e8s;
        if disburse_to_neuron.amount_e8s < min_stake + transaction_fee_e8s {
            return Err(GovernanceError::new_with_message(
                ErrorType::InsufficientFunds,
                format!(
                    "Called `disburse_to_neuron` with `amount` argument {} e8s. This is too little: \
                      at the minimum, one needs the minimum neuron stake, which is {} e8s, \
                      plus the transaction fee, which is {}. Hence the mininum disburse amount is {}.",
                    disburse_to_neuron.amount_e8s,
                    min_stake,
                    transaction_fee_e8s,
                    min_stake + transaction_fee_e8s
                ),
            ));
        }

        if parent_neuron.minted_stake_e8s()
            < economics.neuron_minimum_stake_e8s + disburse_to_neuron.amount_e8s
        {
            return Err(GovernanceError::new_with_message(
                ErrorType::InsufficientFunds,
                format!(
                    "Trying to disburse {} e8s out of neuron {}. \
                     This is not allowed, because the parent has stake {} e8s. \
                     If the requested amount was subtracted from it, there would be less than \
                     the minimum allowed stake, which is {} e8s. ",
                    disburse_to_neuron.amount_e8s,
                    parent_nid.id,
                    parent_neuron.minted_stake_e8s(),
                    min_stake
                ),
            ));
        }

        let state = parent_neuron.state(creation_timestamp_seconds);
        if state != NeuronState::Dissolved {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!(
                    "Neuron {} has NOT been dissolved. It is in state {:?}",
                    id.id, state
                ),
            ));
        }

        if !parent_neuron.kyc_verified {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!("Neuron is not kyc verified: {}", id.id),
            ));
        }

        // Validate that if a child neuron controller was provided, it is a valid
        // principal.
        let child_controller = &disburse_to_neuron
            .new_controller
            .as_ref()
            .ok_or_else(|| {
                GovernanceError::new_with_message(
                    ErrorType::InvalidCommand,
                    "Must specify a new controller for disburse to neuron.",
                )
            })?
            .clone();

        if !child_controller.is_self_authenticating() {
            return Err(GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                "Child neuron controller for disburse neuron must be self-authenticating",
            ));
        }

        let child_nid = self.new_neuron_id();
        let from_subaccount = subaccount_from_slice(&parent_neuron.account)?.ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "From subaccount not specified",
            )
        })?;

        // The account is derived from the new owner's principal so it can be found by
        // the owner on the ledger. There is no need to length-prefix the
        // principal since the nonce is constant length, and so there is no risk
        // of ambiguity.
        let to_subaccount = Subaccount({
            let mut state = Sha256::new();
            state.write(&[0x0c]);
            state.write(b"neuron-split");
            state.write(child_controller.as_slice());
            state.write(&disburse_to_neuron.nonce.to_be_bytes());
            state.finish()
        });

        // Make sure there isn't already a neuron with the same sub-account.
        if self
            .proto
            .neurons
            .values()
            .any(|n| n.account == to_subaccount.0)
        {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "There is already a neuron with the same subaccount.",
            ));
        }

        let in_flight_command = NeuronInFlightCommand {
            timestamp: creation_timestamp_seconds,
            command: Some(InFlightCommand::DisburseToNeuron(
                disburse_to_neuron.clone(),
            )),
        };

        // Make sure the parent neuron is not already undergoing a ledger update.
        let _parent_lock =
            self.lock_neuron_for_command(parent_nid.id, in_flight_command.clone())?;

        let dissolve_delay_seconds = std::cmp::min(
            disburse_to_neuron.dissolve_delay_seconds,
            MAX_DISSOLVE_DELAY_SECONDS,
        );

        // Before we do the transfer, we need to save the neuron in the map
        // otherwise a trap after the transfer is successful but before this
        // method finishes would cause the funds to be lost.
        // However the new neuron is not yet ready to be used as we can't know
        // whether the transfer will succeed, so we temporarily set the
        // stake to 0 and only change it after the transfer is successful.
        let child_neuron = Neuron {
            id: Some(child_nid.clone()),
            account: to_subaccount.to_vec(),
            controller: Some(*child_controller),
            hot_keys: Vec::new(),
            cached_neuron_stake_e8s: 0,
            neuron_fees_e8s: 0,
            created_timestamp_seconds: creation_timestamp_seconds,
            aging_since_timestamp_seconds: creation_timestamp_seconds,
            dissolve_state: Some(DissolveState::DissolveDelaySeconds(dissolve_delay_seconds)),
            followees: self.proto.default_followees.clone(),
            recent_ballots: Vec::new(),
            kyc_verified: disburse_to_neuron.kyc_verified,
            transfer: None,
            maturity_e8s_equivalent: 0,
            staked_maturity_e8s_equivalent: None,
            auto_stake_maturity: None,
            not_for_profit: false,
            joined_community_fund_timestamp_seconds: None,
            known_neuron_data: None,
            spawn_at_timestamp_seconds: None,
        };

        self.add_neuron(child_nid.id, child_neuron.clone())?;

        // Add the child neuron to the set of neurons undergoing ledger updates.
        let _child_lock = self.lock_neuron_for_command(child_nid.id, in_flight_command.clone())?;

        let staked_amount = disburse_to_neuron.amount_e8s - transaction_fee_e8s;

        // Do the transfer from the parent neuron's subaccount to the child neuron's
        // subaccount.
        let memo = creation_timestamp_seconds;
        let result: Result<u64, NervousSystemError> = self
            .ledger
            .transfer_funds(
                staked_amount,
                transaction_fee_e8s,
                Some(from_subaccount),
                neuron_subaccount(to_subaccount),
                memo,
            )
            .await;

        if let Err(error) = result {
            let error = GovernanceError::from(error);
            // If we've got an error, we assume the transfer didn't happen for
            // some reason. The only state to cleanup is to delete the child
            // neuron, since we haven't mutated the parent yet.
            self.remove_neuron(child_nid.id, child_neuron)?;
            println!(
                "Neuron minting transfer of to neuron: {:?}\
                                  failed with error: {:?}. Neuron can't be staked.",
                child_nid, error
            );
            return Err(error);
        }

        // Get the neurons again, but this time mutable references.
        let parent_neuron = self.get_neuron_mut(id).expect("Neuron not found");

        // Update the state of the parent and child neurons.
        parent_neuron.cached_neuron_stake_e8s -= disburse_to_neuron.amount_e8s;

        let child_neuron = self
            .get_neuron_mut(&child_nid)
            .expect("Expected the child neuron to exist");

        child_neuron.cached_neuron_stake_e8s = staked_amount;
        Ok(child_nid)
    }

    /// Set the status of a proposal that is 'being executed' to
    /// 'executed' or 'failed' depending on the value of 'succcess'.
    ///
    /// The proposal ID 'pid' is taken as a raw integer to avoid
    /// lifetime issues.
    pub fn set_proposal_execution_status(&mut self, pid: u64, result: Result<(), GovernanceError>) {
        match self.proto.proposals.get_mut(&pid) {
            Some(mut proposal) => {
                // The proposal has to be adopted before it is executed.
                assert!(proposal.status() == ProposalStatus::Adopted);
                match result {
                    Ok(_) => {
                        println!("Execution of proposal: {} succeeded.", pid);
                        // The proposal was executed 'now'.
                        proposal.executed_timestamp_seconds = self.env.now();
                        // If the proposal previously failed to be
                        // executed, it is no longer that case that the
                        // proposal failed to be executed.
                        proposal.failed_timestamp_seconds = 0;
                        proposal.failure_reason = None;
                    }
                    Err(error) => {
                        println!("Execution of proposal: {} failed. Reason: {:?}", pid, error);
                        // Only update the failure timestamp is there is
                        // not yet any report of success in executing this
                        // proposal. If success already has been reported,
                        // it may be that the failure is reported after
                        // the success, e.g., due to a retry.
                        if proposal.executed_timestamp_seconds == 0 {
                            proposal.failed_timestamp_seconds = self.env.now();
                            proposal.failure_reason = Some(error);
                        }
                    }
                }
            }
            None => {
                // The proposal ID was not found. Something is wrong:
                // just log this information to aid debugging.
                println!(
                    "{}Proposal {:?} not found when attempt to set execution result to {:?}",
                    LOG_PREFIX, pid, result
                );
            }
        }
    }

    /// Returns the neuron info for a given neuron `id`. This method
    /// does not require authorization, so the `NeuronInfo` of a
    /// neuron is accessible to any caller.
    pub fn get_neuron_info(&self, id: &NeuronId) -> Result<NeuronInfo, GovernanceError> {
        let neuron = self
            .proto
            .neurons
            .get(&id.id)
            .ok_or_else(|| GovernanceError::new(ErrorType::NotFound))?;
        let now = self.env.now();
        Ok(neuron.get_neuron_info(now))
    }

    /// Returns the neuron info for a neuron identified by id or subaccount.
    /// This method does not require authorization, so the `NeuronInfo` of a
    /// neuron is accessible to any caller.
    pub fn get_neuron_info_by_id_or_subaccount(
        &self,
        by: &NeuronIdOrSubaccount,
    ) -> Result<NeuronInfo, GovernanceError> {
        let neuron = self.find_neuron(by)?;
        let now = self.env.now();
        Ok(neuron.get_neuron_info(now))
    }

    /// Returns the complete neuron data for a given neuron `id` or
    /// `subaccount` after checking that the `caller` is authorized. The
    /// neuron's controller and hot keys are authorized, as are the
    /// controllers and hot keys of any neurons that are listed as followees
    /// of the requested neuron on the `ManageNeuron` topic.
    pub fn get_full_neuron_by_id_or_subaccount(
        &self,
        by: &NeuronIdOrSubaccount,
        caller: &PrincipalId,
    ) -> Result<Neuron, GovernanceError> {
        let neuron = self.find_neuron(by)?;
        // Check that the caller is authorized for the requested
        // neuron (controller or hot key).
        if !neuron.is_authorized_to_vote(caller) {
            // If not, check if the caller is authorized for any of
            // the followees of the requested neuron.
            let authorized = &mut false;
            if let Some(followees) = neuron.neuron_managers() {
                for f in followees.iter() {
                    if let Some(f_neuron) = self.proto.neurons.get(&f.id) {
                        if f_neuron.is_authorized_to_vote(caller) {
                            *authorized = true;
                            break;
                        }
                    }
                }
            }
            if !*authorized {
                return Err(GovernanceError::new(ErrorType::NotAuthorized));
            }
        }
        Ok(neuron.clone())
    }

    /// Returns the complete neuron data for a given neuron `id` after
    /// checking that the `caller` is authorized. The neuron's
    /// controller and hot keys are authorized, as are the controllers
    /// and hot keys of any neurons that are listed as followees of
    /// the requested neuron on the `ManageNeuron` topic.
    pub fn get_full_neuron(
        &self,
        id: &NeuronId,
        caller: &PrincipalId,
    ) -> Result<Neuron, GovernanceError> {
        self.get_full_neuron_by_id_or_subaccount(
            &NeuronIdOrSubaccount::NeuronId(id.clone()),
            caller,
        )
    }

    // Returns the set of currently registered node providers.
    pub fn get_node_providers(&self) -> &[NodeProvider] {
        &self.proto.node_providers
    }

    pub fn latest_reward_event(&self) -> &RewardEvent {
        self.proto
            .latest_reward_event
            .as_ref()
            .expect("Invariant violation! There should always be a latest_reward_event.")
    }

    /// Tries to get a proposal given a proposal id
    ///
    /// - The proposal's ballots only show votes from neurons that the
    /// caller either controls or is a registered hot key for.
    pub fn get_proposal_info(
        &self,
        caller: &PrincipalId,
        pid: impl Into<ProposalId>,
    ) -> Option<ProposalInfo> {
        let proposal_data = self.proto.proposals.get(&pid.into().id);
        match proposal_data {
            None => None,
            Some(pd) => {
                let empty = HashSet::<u64>::new();
                let caller_neurons: &HashSet<u64> = self
                    .principal_to_neuron_ids_index
                    .get(caller)
                    .unwrap_or(&empty);
                let now = self.env.now();
                Some(self.proposal_data_to_info(pd, caller_neurons, now, false))
            }
        }
    }

    /// Gets all open proposals
    ///
    /// - The proposals' ballots only show votes from neurons that the
    /// caller either controls or is a registered hot key for.
    ///
    /// - Proposals with `ExecuteNnsFunction` as action have their
    /// `payload` cleared if larger than
    /// EXECUTE_NNS_FUNCTION_PAYLOAD_LISTING_BYTES_MAX.  The caller can
    /// retrieve dropped payloads by calling `get_proposal_info` for
    /// each proposal of interest.
    pub fn get_pending_proposals(&self, caller: &PrincipalId) -> Vec<ProposalInfo> {
        let empty = HashSet::<u64>::new();
        let caller_neurons: &HashSet<u64> = self
            .principal_to_neuron_ids_index
            .get(caller)
            .unwrap_or(&empty);
        let now = self.env.now();
        self.get_pending_proposals_data()
            .map(|data| self.proposal_data_to_info(data, caller_neurons, now, true))
            .collect()
    }

    /// Iterator over proposals info of pending proposals.
    pub fn get_pending_proposals_data(&self) -> impl Iterator<Item = &ProposalData> {
        self.proto
            .proposals
            .values()
            .filter(|data| data.status() == ProposalStatus::Open)
    }

    // Gets the raw proposal data
    pub fn get_proposal_data(&self, pid: impl Into<ProposalId>) -> Option<&ProposalData> {
        self.proto.proposals.get(&pid.into().id)
    }

    fn mut_proposal_data(&mut self, pid: impl Into<ProposalId>) -> Option<&mut ProposalData> {
        self.proto.proposals.get_mut(&pid.into().id)
    }

    fn proposal_data_to_info(
        &self,
        data: &ProposalData,
        caller_neurons: &HashSet<u64>,
        now_seconds: u64,
        multi_query: bool,
    ) -> ProposalInfo {
        // Calculate derived fields
        let topic = data.topic();
        let status = data.status();
        let voting_period_seconds = self.voting_period_seconds()(topic);
        let reward_status = data.reward_status(now_seconds, voting_period_seconds);

        // If this is part of a "multi" query and an ExecuteNnsFunction
        // proposal then remove the payload if the payload is larger
        // than EXECUTE_NNS_FUNCTION_PAYLOAD_LISTING_BYTES_MAX.
        let mut new_proposal = data.proposal.clone();
        if multi_query {
            if let Some(proposal) = &mut new_proposal {
                if let Some(proposal::Action::ExecuteNnsFunction(m)) = &mut proposal.action {
                    if m.payload.len() > EXECUTE_NNS_FUNCTION_PAYLOAD_LISTING_BYTES_MAX {
                        m.payload.clear();
                    }
                }
            }
        }

        /// Remove all ballots except the ballots belonging to a neuron present
        /// in `except_from`.
        fn remove_ballots_not_cast_by(
            all_ballots: &HashMap<u64, Ballot>,
            except_from: &HashSet<u64>,
        ) -> HashMap<u64, Ballot> {
            let mut ballots = HashMap::new();
            for n in except_from.iter() {
                if let Some(v) = all_ballots.get(n) {
                    ballots.insert(*n, v.clone());
                }
            }
            ballots
        }

        ProposalInfo {
            id: data.id,
            proposer: data.proposer.clone(),
            reject_cost_e8s: data.reject_cost_e8s,
            proposal: new_proposal,
            proposal_timestamp_seconds: data.proposal_timestamp_seconds,
            ballots: remove_ballots_not_cast_by(&data.ballots, caller_neurons),
            latest_tally: data.latest_tally.clone(),
            decided_timestamp_seconds: data.decided_timestamp_seconds,
            executed_timestamp_seconds: data.executed_timestamp_seconds,
            failed_timestamp_seconds: data.failed_timestamp_seconds,
            failure_reason: data.failure_reason.clone(),
            reward_event_round: data.reward_event_round,
            topic: topic as i32,
            status: status as i32,
            reward_status: reward_status as i32,
            deadline_timestamp_seconds: Some(
                data.get_deadline_timestamp_seconds(voting_period_seconds),
            ),
            derived_proposal_information: data.derived_proposal_information.clone(),
        }
    }

    /// Return true if the 'info' proposal is visible to some of the neurons in
    /// 'caller_neurons'.
    fn proposal_is_visible_to_neurons(
        &self,
        info: &ProposalData,
        caller_neurons: &HashSet<u64>,
    ) -> bool {
        // Is 'info' a manage neuron proposal?
        if let Some(ref managed_id) = info.proposal.as_ref().and_then(|x| x.managed_neuron()) {
            // mgr_ids: &Vec<NeuronId>
            if let Some(mgr_ids) = self
                .find_neuron(managed_id)
                .ok()
                .and_then(|x| x.neuron_managers())
            {
                // Find one ID in the list of manager IDs that is also
                // in 'caller_neurons'.
                if mgr_ids.iter().any(|x| caller_neurons.contains(&x.id)) {
                    // If such an ID is found, the caller is
                    // permitted to list this proposal.
                    return true;
                }
                // 'caller' not authorized
                false
            } else {
                // This proposal is 'managed', but we cannot find out
                // by whom - don't show.
                false
            }
        } else {
            // This proposal is is not 'managed' - fine to show in all lists.
            true
        }
    }

    /// Returns the proposals info of proposals with proposal ID less
    /// than `before_proposal` (exclusive), returning at most `limit` proposal
    /// infos. If `before_proposal` is not provided, start from the highest
    /// available proposal ID (inclusive).
    ///
    /// As proposal IDs are assigned sequentially, this retrives up to
    /// `limit` proposals older (in terms of creation) than a specific
    /// proposal. This can be used to paginate through proposals, as follows:
    ///
    /// `
    /// let mut lst = gov.list_proposals(ListProposalInfo {});
    /// while !lst.empty() {
    ///   /* do stuff with lst */
    ///   lst = gov.list_proposals(ListProposalInfo {
    ///     before_proposal: lst.last().and_then(|x|x.id)
    ///   });
    /// }
    /// `
    ///
    /// - A proposal with resticted voting is included only if the
    /// caller is allowed to vote on the proposal.
    ///
    /// - The proposals' ballots only show votes from neurons that the
    /// caller either controls or is a registered hot key for.
    ///
    /// - Proposals with `ExecuteNnsFunction` as action have their
    /// `payload` cleared if larger than
    /// EXECUTE_NNS_FUNCTION_PAYLOAD_LISTING_BYTES_MAX.  The caller can
    /// retrieve dropped payloads by calling `get_proposal_info` for
    /// each proposal of interest.
    pub fn list_proposals(
        &self,
        caller: &PrincipalId,
        req: &ListProposalInfo,
    ) -> ListProposalInfoResponse {
        let empty = HashSet::<u64>::new();
        let caller_neurons: &HashSet<u64> = self
            .principal_to_neuron_ids_index
            .get(caller)
            .unwrap_or(&empty);
        let exclude_topic: HashSet<i32> = req.exclude_topic.iter().cloned().collect();
        let include_reward_status: HashSet<i32> =
            req.include_reward_status.iter().cloned().collect();
        let include_status: HashSet<i32> = req.include_status.iter().cloned().collect();
        let now = self.env.now();
        let filter_all = |data: &ProposalData| -> bool {
            let topic = data.topic();
            let voting_period_seconds = self.voting_period_seconds()(topic);
            // Filter out proposals by topic.
            if exclude_topic.contains(&(topic as i32)) {
                return false;
            }
            // Filter out proposals by reward status.
            if !(include_reward_status.is_empty()
                || include_reward_status
                    .contains(&(data.reward_status(now, voting_period_seconds) as i32)))
            {
                return false;
            }
            // Filter out proposals by status.
            if !(include_status.is_empty() || include_status.contains(&(data.status() as i32))) {
                return false;
            }
            // Filter out proposals by their restricted status.
            self.proposal_is_visible_to_neurons(data, caller_neurons)
        };
        let limit = if req.limit == 0 || req.limit > MAX_LIST_PROPOSAL_RESULTS {
            MAX_LIST_PROPOSAL_RESULTS
        } else {
            req.limit
        } as usize;
        let props = &self.proto.proposals;
        // Proposals are stored in a sorted map. If 'before_proposal'
        // is provided, grab all proposals before that, else grab the
        // whole range.
        let rng = if let Some(n) = req.before_proposal {
            props.range(..(n.id))
        } else {
            props.range(..)
        };
        // Now reverse the range, filter, and restrict to 'limit'.
        let limited_rng = rng.rev().filter(|(_, x)| filter_all(x)).take(limit);
        //
        let proposal_info = limited_rng
            .map(|(_, y)| y)
            .map(|pd| self.proposal_data_to_info(pd, caller_neurons, now, true))
            .collect();
        // Ignore the keys and clone to a vector.
        ListProposalInfoResponse { proposal_info }
    }

    fn ready_to_be_settled_proposal_ids(&self) -> impl Iterator<Item = ProposalId> + '_ {
        let now = self.env.now();
        self.proto
            .proposals
            .iter()
            .filter(move |(_, data)| {
                let topic = data.topic();
                let voting_period_seconds = self.voting_period_seconds()(topic);
                data.reward_status(now, voting_period_seconds)
                    == ProposalRewardStatus::ReadyToSettle
            })
            .map(|(k, _)| ProposalId { id: *k })
    }

    pub fn num_ready_to_be_settled_proposals(&self) -> usize {
        self.ready_to_be_settled_proposal_ids().count()
    }

    /// This method attempts to move a proposal forward in the process,
    /// from open to adopted or rejected, to executed or failed (for a
    /// previously adopted proposal).
    ///
    /// If the proposal is open, it tallies the ballots and updates the
    /// `yes`, `no`, and `undecided` voting power accordingly.
    ///
    /// This may result in the proposal becoming adopted or rejected.
    ///
    /// If a proposal is adopted but not executed, this method
    /// attempts to execute it.
    pub fn process_proposal(&mut self, pid: u64) {
        let now_seconds = self.env.now();
        // Due to Rust lifetime issues, we must extract a closure that
        // computes the voting period from a topic before we borrow
        // `self.proto` mutably.
        let voting_period_seconds_fn = self.voting_period_seconds();
        if let Some(p) = self.proto.proposals.get_mut(&pid) {
            if p.status() != ProposalStatus::Open {
                return;
            }
            let topic = p.topic();
            let voting_period_seconds = voting_period_seconds_fn(topic);
            // Recompute the tally here. It is imperative that only
            // 'open' proposals have their tally recomputed. Votes may
            // arrive after a decision has been made: such votes count
            // for voting rewards, but shall not make it into the
            // tally.
            p.recompute_tally(now_seconds, voting_period_seconds);

            if !p.can_make_decision(now_seconds, voting_period_seconds) {
                return;
            }
            // This marks the proposal as no longer open.
            p.decided_timestamp_seconds = now_seconds;
            if !p.is_accepted() {
                self.start_process_rejected_proposal(pid);
                return;
            }
            // The proposal was adopted, return the rejection fee for non-ManageNeuron
            // proposals.
            if !p
                .proposal
                .as_ref()
                .map(|x| x.is_manage_neuron())
                .unwrap_or(false)
            {
                if let Some(nid) = &p.proposer {
                    if let Some(neuron) = self.proto.neurons.get_mut(&nid.id) {
                        if neuron.neuron_fees_e8s >= p.reject_cost_e8s {
                            neuron.neuron_fees_e8s -= p.reject_cost_e8s;
                        }
                    }
                }
            }
            let original_total_community_fund_maturity_e8s_equivalent =
                p.original_total_community_fund_maturity_e8s_equivalent;
            if let Some(action) = p.proposal.as_ref().and_then(|x| x.action.clone()) {
                // A yes decision as been made, execute the proposal!
                self.start_proposal_execution(
                    pid,
                    &action,
                    original_total_community_fund_maturity_e8s_equivalent,
                );
            } else {
                self.set_proposal_execution_status(
                    pid,
                    Err(GovernanceError::new_with_message(
                        ErrorType::PreconditionFailed,
                        "Proposal is missing.",
                    )),
                );
            }
        }
    }

    /// Process all the open proposals.
    fn process_proposals(&mut self) {
        if self.env.now() < self.closest_proposal_deadline_timestamp_seconds {
            // Nothing to do.
            return;
        }

        let pids = self
            .proto
            .proposals
            .iter()
            .filter(|(_, info)| info.status() == ProposalStatus::Open)
            .map(|(pid, _)| *pid)
            .collect::<Vec<u64>>();

        for pid in pids {
            self.process_proposal(pid);
        }

        self.closest_proposal_deadline_timestamp_seconds = self
            .proto
            .proposals
            .values()
            .filter(|data| data.status() == ProposalStatus::Open)
            .map(|data| {
                data.proposal_timestamp_seconds
                    .saturating_add(self.voting_period_seconds()(data.topic()))
            })
            .min()
            .unwrap_or(u64::MAX);
    }

    fn start_process_rejected_proposal(&mut self, pid: u64) {
        // Similar method to "start_proposal_execution"
        // `process_rejected_proposal` is an async method of &mut self.
        //
        // Starting it and letting it run in the background requires knowing that
        // the `self` reference will last until the future has completed.
        //
        // The compiler cannot know that, but this is actually true:
        //
        // - in unit tests, all futures are immediately ready, because no real async
        //   call is made. In this case, the transmutation to a static ref is abusive,
        //   but it's still ok since the future will immediately resolve.
        //
        // - in prod, "self" in a reference to the GOVERNANCE static variable, which is
        //   initialized only once (in canister_init or canister_post_upgrade)
        let governance: &'static mut Governance = unsafe { std::mem::transmute(self) };
        spawn(governance.process_rejected_proposal(pid));
    }

    async fn process_rejected_proposal(&mut self, pid: u64) {
        let proposal_data = match self.proto.proposals.get(&pid) {
            None => {
                println!(".");
                return;
            }
            Some(p) => p,
        };

        if let Some(Action::OpenSnsTokenSwap(ref open_sns_token_swap)) = proposal_data
            .proposal
            .as_ref()
            .and_then(|p| p.action.clone())
        {
            self.process_rejected_open_sns_token_swap(open_sns_token_swap)
                .await
        }
    }

    async fn process_rejected_open_sns_token_swap(
        &mut self,
        open_sns_token_swap: &OpenSnsTokenSwap,
    ) {
        let request = RestoreDappControllersRequest {};

        let target_swap_canister_id = open_sns_token_swap
            .target_swap_canister_id
            .expect("No value in the target_swap_canister_id field.")
            .try_into()
            .expect("Unable to convert target_swap_canister_id into a CanisterId.");

        let _result = self
            .env
            .call_canister_method(
                target_swap_canister_id,
                "restore_dapp_controllers",
                Encode!(&request).expect("Unable to encode RestoreDappControllersRequest."),
            )
            .await;
    }

    /// Starts execution of the given proposal in the background.
    fn start_proposal_execution(
        &mut self,
        pid: u64,
        action: &proposal::Action,
        original_total_community_fund_maturity_e8s_equivalent: Option<u64>,
    ) {
        // `perform_action` is an async method of &mut self.
        //
        // Starting it and letting it run in the background requires knowing that
        // the `self` reference will last until the future has completed.
        //
        // The compiler cannot know that, but this is actually true:
        //
        // - in unit tests, all futures are immediately ready, because no real async
        //   call is made. In this case, the transmutation to a static ref is abusive,
        //   but it's still ok since the future will immediately resolve.
        //
        // - in prod, "self" in a reference to the GOVERNANCE static variable, which is
        //   initialized only once (in canister_init or canister_post_upgrade)
        let governance: &'static mut Governance = unsafe { std::mem::transmute(self) };
        spawn(governance.perform_action(
            pid,
            action.clone(),
            original_total_community_fund_maturity_e8s_equivalent,
        ));
    }

    /// Mints node provider rewards to a neuron or to a ledger account.
    async fn mint_reward_to_neuron_or_account(
        &mut self,
        np_principal: &PrincipalId,
        reward: &RewardNodeProvider,
    ) -> Result<(), GovernanceError> {
        let now = self.env.now();
        match reward.reward_mode.as_ref() {
            None => Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Reward node provider proposal must have a reward mode.",
            )),
            Some(RewardMode::RewardToNeuron(reward_to_neuron)) => {
                let to_subaccount = Subaccount(self.env.random_byte_array());
                let _block_height = self
                    .ledger
                    .transfer_funds(
                        reward.amount_e8s,
                        0, // Minting transfers don't pay transaction fees.
                        None,
                        neuron_subaccount(to_subaccount),
                        now,
                    )
                    .await?;
                let nid = self.new_neuron_id();
                let dissolve_delay_seconds = std::cmp::min(
                    reward_to_neuron.dissolve_delay_seconds,
                    MAX_DISSOLVE_DELAY_SECONDS,
                );
                // Transfer successful.
                let neuron = Neuron {
                    id: Some(nid.clone()),
                    account: to_subaccount.to_vec(),
                    controller: Some(*np_principal),
                    hot_keys: Vec::new(),
                    cached_neuron_stake_e8s: reward.amount_e8s,
                    neuron_fees_e8s: 0,
                    created_timestamp_seconds: now,
                    aging_since_timestamp_seconds: now,
                    dissolve_state: Some(DissolveState::DissolveDelaySeconds(
                        dissolve_delay_seconds,
                    )),
                    followees: self.proto.default_followees.clone(),
                    recent_ballots: vec![],
                    kyc_verified: true,
                    maturity_e8s_equivalent: 0,
                    staked_maturity_e8s_equivalent: None,
                    auto_stake_maturity: None,
                    not_for_profit: false,
                    transfer: None,
                    joined_community_fund_timestamp_seconds: None,
                    known_neuron_data: None,
                    spawn_at_timestamp_seconds: None,
                };
                self.add_neuron(nid.id, neuron)
            }
            Some(RewardMode::RewardToAccount(reward_to_account)) => {
                // We are not creating a neuron, just transferring funds.
                let to_account = match &reward_to_account.to_account {
                    Some(to_account) => AccountIdentifier::try_from(to_account).map_err(|e| {
                        GovernanceError::new_with_message(
                            ErrorType::InvalidCommand,
                            format!("The recipient's subaccount is invalid due to: {}", e),
                        )
                    })?,
                    None => AccountIdentifier::new(*np_principal, None),
                };

                self.ledger
                    .transfer_funds(
                        reward.amount_e8s,
                        0, // Minting transfers don't pay transaction fees.
                        None,
                        to_account,
                        now,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|e| {
                        GovernanceError::new_with_message(
                            ErrorType::PreconditionFailed,
                            format!(
                                "Couldn't perform minting transfer: {}",
                                GovernanceError::from(e)
                            ),
                        )
                    })
            }
        }
    }

    async fn reward_node_provider_helper(
        &mut self,
        reward: &RewardNodeProvider,
    ) -> Result<(), GovernanceError> {
        if let Some(node_provider) = &reward.node_provider {
            if let Some(np_principal) = &node_provider.id {
                if !self
                    .proto
                    .node_providers
                    .iter()
                    .any(|np| np.id == node_provider.id)
                {
                    Err(GovernanceError::new_with_message(
                        ErrorType::NotFound,
                        format!("Node provider with id {} not found.", np_principal),
                    ))
                } else {
                    // Check that the amount to distribute is not above
                    // than the maximum set in network economics.
                    let maximum_node_provider_rewards_e8s =
                        self.economics().maximum_node_provider_rewards_e8s;
                    if reward.amount_e8s > maximum_node_provider_rewards_e8s {
                        Err(GovernanceError::new_with_message(
                            ErrorType::PreconditionFailed,
                            format!(
                                "Proposed reward {} greater than maximum {}",
                                reward.amount_e8s, maximum_node_provider_rewards_e8s
                            ),
                        ))
                    } else {
                        self.mint_reward_to_neuron_or_account(np_principal, reward)
                            .await
                    }
                }
            } else {
                Err(GovernanceError::new_with_message(
                    ErrorType::PreconditionFailed,
                    "Node provider has no ID.",
                ))
            }
        } else {
            Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Proposal was missing the node provider.",
            ))
        }
    }

    /// Rewards a node provider.
    async fn reward_node_provider(&mut self, pid: u64, reward: &RewardNodeProvider) {
        let result = self.reward_node_provider_helper(reward).await;
        self.set_proposal_execution_status(pid, result);
    }

    /// Mint and transfer the specified Node Provider rewards
    async fn reward_node_providers(
        &mut self,
        rewards: Vec<RewardNodeProvider>,
    ) -> Result<(), GovernanceError> {
        let mut result = Ok(());

        for reward in rewards {
            let reward_result = self.reward_node_provider_helper(&reward).await;
            if reward_result.is_err() {
                println!(
                    "Rewarding {:?} failed. Reason: {:}",
                    reward,
                    reward_result.clone().unwrap_err()
                );
            }
            result = result.or(reward_result);
        }

        result
    }

    /// Execute a RewardNodeProviders proposal
    async fn reward_node_providers_from_proposal(
        &mut self,
        pid: u64,
        reward_nps: RewardNodeProviders,
    ) {
        let result = if reward_nps.use_registry_derived_rewards == Some(true) {
            self.mint_monthly_node_provider_rewards().await
        } else {
            self.reward_node_providers(reward_nps.rewards).await
        };

        self.set_proposal_execution_status(pid, result);
    }

    /// Return `true` if `NODE_PROVIDER_REWARD_PERIOD_SECONDS` has passed since the last monthly
    /// node provider reward event
    fn is_time_to_mint_monthly_node_provider_rewards(&self) -> bool {
        match &self.proto.most_recent_monthly_node_provider_rewards {
            None => false,
            Some(recent_rewards) => {
                self.env.now().saturating_sub(recent_rewards.timestamp)
                    >= NODE_PROVIDER_REWARD_PERIOD_SECONDS
            }
        }
    }

    /// Mint and transfer monthly node provider rewards
    async fn mint_monthly_node_provider_rewards(&mut self) -> Result<(), GovernanceError> {
        let rewards = self.get_monthly_node_provider_rewards().await?.rewards;
        let _ = self.reward_node_providers(rewards.clone()).await;
        self.update_most_recent_monthly_node_provider_rewards(rewards);

        Ok(())
    }

    fn update_most_recent_monthly_node_provider_rewards(
        &mut self,
        rewards: Vec<RewardNodeProvider>,
    ) {
        let most_recent_rewards = MostRecentMonthlyNodeProviderRewards {
            timestamp: self.env.now(),
            rewards,
        };

        self.proto.most_recent_monthly_node_provider_rewards = Some(most_recent_rewards);
    }

    async fn perform_action(
        &mut self,
        pid: u64,
        action: proposal::Action,
        original_total_community_fund_maturity_e8s_equivalent: Option<u64>,
    ) {
        match action {
            proposal::Action::ManageNeuron(mgmt) => {
                // An adopted neuron management command is executed
                // with the privileges of the controller of the
                // neuron.
                match mgmt.get_neuron_id_or_subaccount() {
                    Ok(Some(ref managed_neuron_id)) => {
                        if let Some(controller) = self
                            .find_neuron(managed_neuron_id)
                            .ok()
                            .and_then(|x| x.controller.as_ref())
                            .copied()
                        {
                            let result = self.manage_neuron(&controller, &mgmt).await;
                            match result.command {
                                Some(manage_neuron_response::Command::Error(err)) => {
                                    self.set_proposal_execution_status(pid, Err(err))
                                }
                                _ => self.set_proposal_execution_status(pid, Ok(())),
                            };
                        } else {
                            self.set_proposal_execution_status(
                                pid,
                                Err(GovernanceError::new_with_message(
                                    ErrorType::NotAuthorized,
                                    "Couldn't execute manage neuron proposal.\
                                          The neuron doesn't have a controller.",
                                )),
                            );
                        }
                    }
                    Ok(None) => {
                        self.set_proposal_execution_status(
                            pid,
                            Err(GovernanceError::new_with_message(
                                ErrorType::NotFound,
                                "Couldn't execute manage neuron proposal.\
                                          The neuron was not found.",
                            )),
                        );
                    }
                    Err(e) => self.set_proposal_execution_status(pid, Err(e)),
                }
            }
            proposal::Action::ManageNetworkEconomics(ne) => {
                if let Some(economics) = &mut self.proto.economics {
                    // The semantics of the proposal is to modify all values specified with a
                    // non-default value in the proposed new `NetworkEconomics`.
                    if ne.reject_cost_e8s != 0 {
                        economics.reject_cost_e8s = ne.reject_cost_e8s
                    }
                    if ne.neuron_minimum_stake_e8s != 0 {
                        economics.neuron_minimum_stake_e8s = ne.neuron_minimum_stake_e8s
                    }
                    if ne.neuron_management_fee_per_proposal_e8s != 0 {
                        economics.neuron_management_fee_per_proposal_e8s =
                            ne.neuron_management_fee_per_proposal_e8s
                    }
                    if ne.minimum_icp_xdr_rate != 0 {
                        economics.minimum_icp_xdr_rate = ne.minimum_icp_xdr_rate
                    }
                    if ne.neuron_spawn_dissolve_delay_seconds != 0 {
                        economics.neuron_spawn_dissolve_delay_seconds =
                            ne.neuron_spawn_dissolve_delay_seconds
                    }
                    if ne.maximum_node_provider_rewards_e8s != 0 {
                        economics.maximum_node_provider_rewards_e8s =
                            ne.maximum_node_provider_rewards_e8s
                    }
                    if ne.transaction_fee_e8s != 0 {
                        economics.transaction_fee_e8s = ne.transaction_fee_e8s
                    }
                    if ne.max_proposals_to_keep_per_topic != 0 {
                        economics.max_proposals_to_keep_per_topic =
                            ne.max_proposals_to_keep_per_topic
                    }
                } else {
                    // If for some reason, we don't have an
                    // 'economics' proto, use the proposed one.
                    self.proto.economics = Some(ne)
                }
                self.set_proposal_execution_status(pid, Ok(()));
            }
            // A motion is not executed, just recorded for posterity.
            proposal::Action::Motion(_) => {
                self.set_proposal_execution_status(pid, Ok(()));
            }
            proposal::Action::ExecuteNnsFunction(m) => {
                // This will eventually set the proposal execution
                // status.
                match self.env.execute_nns_function(pid, &m) {
                    Ok(()) => {
                        // The status will be set as a result of this
                        // call. We don't set it now.
                    }
                    Err(_) => {
                        self.set_proposal_execution_status(
                            pid,
                            Err(GovernanceError::new_with_message(
                                ErrorType::External,
                                "Couldn't execute NNS function through proposal",
                            )),
                        );
                    }
                }
            }
            proposal::Action::ApproveGenesisKyc(proposal) => {
                self.approve_genesis_kyc(&proposal.principals);
                self.set_proposal_execution_status(pid, Ok(()));
            }
            proposal::Action::AddOrRemoveNodeProvider(ref proposal) => {
                if let Some(change) = &proposal.change {
                    match change {
                        Change::ToAdd(node_provider) => {
                            if node_provider.id.is_none() {
                                self.set_proposal_execution_status(
                                    pid,
                                    Err(GovernanceError::new_with_message(
                                        ErrorType::PreconditionFailed,
                                        "Node providers must have a principal id.",
                                    )),
                                );
                                return;
                            }

                            // Check if the node provider already exists
                            if self
                                .proto
                                .node_providers
                                .iter()
                                .any(|np| np.id == node_provider.id)
                            {
                                self.set_proposal_execution_status(
                                    pid,
                                    Err(GovernanceError::new_with_message(
                                        ErrorType::PreconditionFailed,
                                        "A node provider with the same principal already exists.",
                                    )),
                                );
                                return;
                            }
                            self.proto.node_providers.push(node_provider.clone());
                            self.set_proposal_execution_status(pid, Ok(()));
                        }
                        Change::ToRemove(node_provider) => {
                            if node_provider.id.is_none() {
                                self.set_proposal_execution_status(
                                    pid,
                                    Err(GovernanceError::new_with_message(
                                        ErrorType::PreconditionFailed,
                                        "Node providers must have a principal id.",
                                    )),
                                );
                                return;
                            }

                            if let Some(pos) = self
                                .proto
                                .node_providers
                                .iter()
                                .position(|np| np.id == node_provider.id)
                            {
                                self.proto.node_providers.remove(pos);
                                self.set_proposal_execution_status(pid, Ok(()));
                            } else {
                                self.set_proposal_execution_status(
                                    pid,
                                    Err(GovernanceError::new_with_message(
                                        ErrorType::NotFound,
                                        "Can't find a NodeProvider with the same principal id.",
                                    )),
                                );
                            }
                        }
                    }
                } else {
                    self.set_proposal_execution_status(
                        pid,
                        Err(GovernanceError::new_with_message(
                            ErrorType::PreconditionFailed,
                            "The proposal didn't contain a change.",
                        )),
                    );
                }
            }
            proposal::Action::RewardNodeProvider(ref reward) => {
                self.reward_node_provider(pid, reward).await;
            }
            proposal::Action::SetDefaultFollowees(ref proposal) => {
                let validate_result = self
                    .proto
                    .validate_default_followees(&proposal.default_followees);
                if validate_result.is_err() {
                    self.set_proposal_execution_status(pid, validate_result);
                    return;
                }
                self.proto.default_followees = proposal.default_followees.clone();
                self.set_proposal_execution_status(pid, Ok(()));
            }
            proposal::Action::RewardNodeProviders(proposal) => {
                self.reward_node_providers_from_proposal(pid, proposal)
                    .await;
            }
            proposal::Action::RegisterKnownNeuron(known_neuron) => {
                let result = self.register_known_neuron(known_neuron);
                self.set_proposal_execution_status(pid, result);
            }
            proposal::Action::SetSnsTokenSwapOpenTimeWindow(
                ref set_sns_token_swap_open_time_window,
            ) => self.set_sns_token_swap_open_time_window(pid, set_sns_token_swap_open_time_window),
            proposal::Action::OpenSnsTokenSwap(ref open_sns_token_swap) => {
                self.open_sns_token_swap(
                    pid,
                    open_sns_token_swap,
                    original_total_community_fund_maturity_e8s_equivalent
                        .expect("Missing original_total_community_fund_maturity_e8s_equivalent."),
                )
                .await;
            }
        }
    }

    /// Fails immediately, because this type of proposal is obsolete.
    fn set_sns_token_swap_open_time_window(
        &mut self,
        proposal_id: u64,
        set_sns_token_swap_open_time_window: &SetSnsTokenSwapOpenTimeWindow,
    ) {
        self.set_proposal_execution_status(
            proposal_id,
            Err(GovernanceError::new_with_message(
                ErrorType::InvalidProposal,
                format!(
                    "The SetSnsTokenSwapOpenTimeWindow proposal action is obsolete: {:?}",
                    set_sns_token_swap_open_time_window,
                ),
            )),
        );
    }

    async fn open_sns_token_swap(
        &mut self,
        proposal_id: u64,
        open_sns_token_swap: &OpenSnsTokenSwap,
        original_total_community_fund_maturity_e8s_equivalent: u64,
    ) {
        let params = open_sns_token_swap
            .params
            .as_ref()
            .expect("OpenSnsTokenSwap proposal lacks params.")
            .clone();

        let cf_participants = draw_funds_from_the_community_fund(
            &mut self.proto.neurons,
            original_total_community_fund_maturity_e8s_equivalent,
            open_sns_token_swap
                .community_fund_investment_e8s
                .unwrap_or_default(),
            &params,
        );

        // Record the maturity deductions that we just made.
        match self.proto.proposals.get_mut(&proposal_id) {
            Some(proposal_data) => {
                proposal_data.cf_participants = cf_participants.clone();
            }
            None => {
                let failed_refunds =
                    refund_community_fund_maturity(&mut self.proto.neurons, &cf_participants);
                self.set_proposal_execution_status(
                    proposal_id,
                    Err(GovernanceError::new_with_message(
                        ErrorType::NotFound,
                        format!(
                            "OpenSnsTokenSwap proposal {} not found while trying to execute it. \
                             open_sns_token_swap = {:#?}. failed_refunds = {:#?}",
                            proposal_id, open_sns_token_swap, failed_refunds,
                        ),
                    )),
                );
                return;
            }
        }

        let request = sns_swap_pb::OpenRequest {
            params: Some(params),
            cf_participants: cf_participants.clone(),
            open_sns_token_swap_proposal_id: Some(proposal_id),
        };

        let target_swap_canister_id = open_sns_token_swap
            .target_swap_canister_id
            .expect("No value in the target_swap_canister_id field.")
            .try_into()
            .expect("Unable to convert target_swap_canister_id into a CanisterId.");

        // The main event: call the swap canister's open method.
        let result = self
            .env
            .call_canister_method(
                target_swap_canister_id,
                "open",
                Encode!(&request).expect("Unable to encode OpenRequest."),
            )
            .await;

        if let Err(err) = result {
            let failed_refunds =
                refund_community_fund_maturity(&mut self.proto.neurons, &cf_participants);

            self.set_proposal_execution_status(proposal_id, Err(GovernanceError::new_with_message(
                ErrorType::External,
                format!(
                    "Call to the open method of swap canister {} failed: {:?}. Request was {:#?} \
                    proposal_id = {:?}. open_sns_token_swap = {:#?}. cf_participants = {:#?}. \
                    failed_refunds = {:#?}.",
                    target_swap_canister_id, err, request, proposal_id, open_sns_token_swap, cf_participants, failed_refunds,
                ),
            )));
            return;
        }

        // Call to the swap canister was a success. Record this fact, and return.
        if let Some(proposal_data) = self.proto.proposals.get_mut(&proposal_id) {
            Self::set_sns_token_swap_lifecycle_to_open(proposal_data);
            self.set_proposal_execution_status(proposal_id, Ok(()));
            return;
        }

        // ProposalData not found?!
        println!(
            "{}Unable to find ProposalData {} while executing it.",
            LOG_PREFIX, proposal_id,
        );
        let failed_refunds =
            refund_community_fund_maturity(&mut self.proto.neurons, &cf_participants);
        let result = Err(GovernanceError::new_with_message(
            ErrorType::NotFound,
            format!(
                "OpenSnsTokenSwap proposal not found while trying to execute it. \
                proposal_id = {:?}. open_sns_token_swap = {:#?}. cf_participants = {:#?}. \
                failed_refunds = {:#?}.",
                proposal_id, open_sns_token_swap, cf_participants, failed_refunds,
            ),
        ));
        self.set_proposal_execution_status(proposal_id, result);
    }

    fn set_sns_token_swap_lifecycle_to_open(proposal_data: &mut ProposalData) {
        let lifecycle = &mut proposal_data.sns_token_swap_lifecycle;
        match lifecycle {
            None => {
                *lifecycle = Some(sns_swap_pb::Lifecycle::Open as i32);
            }
            Some(lifecycle) => {
                // This can happen if swap calls
                // conclude_community_fund_participation (and that gets fully
                // processed) before the await returns on the call to the swap's
                // open Candid method. This is unusual, but plausible if the CF
                // participation is high enough to make the swap an immediate.
                // success.
                println!(
                    "{}WARNING: The sns_token_swap_lifecycle field in a ProposalData \
                     has is unexpected already been set to {:?}. Leaving the field as-is.",
                    LOG_PREFIX, lifecycle,
                );
            }
        }
    }

    /// Mark all Neurons controlled by the given principals as having passed
    /// KYC verification
    pub fn approve_genesis_kyc(&mut self, principals: &[PrincipalId]) {
        let principal_set: HashSet<&PrincipalId> = principals.iter().collect();

        for principal in principal_set {
            for neuron_id in self.get_neuron_ids_by_principal(principal) {
                if let Some(neuron) = self.proto.neurons.get_mut(&neuron_id) {
                    if neuron.controller.as_ref() == Some(principal) {
                        neuron.kyc_verified = true;
                    }
                }
            }
        }
    }

    fn make_manage_neuron_proposal(
        &mut self,
        proposer_id: &NeuronId,
        caller: &PrincipalId,
        now_seconds: u64,
        manage_neuron: &ManageNeuron,
        summary: &str,
        url: &str,
    ) -> Result<ProposalId, GovernanceError> {
        // Validate
        let manage_neuron = ManageNeuron::from_proto(manage_neuron.clone()).map_err(|e| {
            GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                format!("Failed to validate ManageNeuron {}", e),
            )
        })?;
        let neuron_management_fee_per_proposal_e8s =
            self.economics().neuron_management_fee_per_proposal_e8s;
        // Find the proposing neuron.
        let proposer = self.proto.neurons.get(&proposer_id.id).ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::NotFound,
                format!("Proposer neuron not found: {}", proposer_id.id),
            )
        })?;
        // Check that the caller is authorized, i.e., either the
        // controller or a registered hot key, to vote on behalf of
        // the proposing neuron.
        if !proposer.is_authorized_to_vote(caller) {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                "Caller not authorized to propose.",
            ));
        }
        let managed_id = manage_neuron
            .get_neuron_id_or_subaccount()?
            .ok_or_else(|| {
                GovernanceError::new_with_message(
                    ErrorType::NotFound,
                    "Proposal must include a neuron to manage.",
                )
            })?;
        let managed_neuron = self.find_neuron(&managed_id)?;

        let command = manage_neuron.command.as_ref().ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "A manage neuron action must have a command",
            )
        })?;

        // Only not-for-profit neurons can issue disburse/split/disburse-to-neuron
        // commands through a proposal.
        if !managed_neuron.not_for_profit {
            match command {
                Command::Disburse(_) => {
                    return Err(GovernanceError::new_with_message(
                        ErrorType::NotAuthorized,
                        "Cannot issue a disburse command through a proposal",
                    ));
                }
                Command::DisburseToNeuron(_) => {
                    return Err(GovernanceError::new_with_message(
                        ErrorType::NotAuthorized,
                        "Cannot issue a disburse to neuron command through a proposal",
                    ));
                }
                _ => (),
            }
        }

        // A neuron can be managed only by its followees on the
        // 'manage neuron' topic.
        let followees = managed_neuron
            .followees
            .get(&(Topic::NeuronManagement as i32))
            .ok_or_else(|| {
                GovernanceError::new_with_message(
                    ErrorType::PreconditionFailed,
                    "Managed neuron does not specify any followees on the 'manage neuron' topic.",
                )
            })?;
        if !followees.followees.iter().any(|x| x.id == proposer_id.id) {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Proposer not among the followees of neuron.",
            ));
        }
        if proposer.minted_stake_e8s() < neuron_management_fee_per_proposal_e8s {
            return Err(
                // Not enough stake to make proposal.
                GovernanceError::new_with_message(
                    ErrorType::InsufficientFunds,
                    "Proposer doesn't have enough minted stake for proposal.",
                ),
            );
        }
        // Check that there are not too many open manage neuron
        // proposals already.
        if self
            .proto
            .proposals
            .values()
            .filter(|info| info.is_manage_neuron() && info.status() == ProposalStatus::Open)
            .count()
            >= MAX_NUMBER_OF_OPEN_MANAGE_NEURON_PROPOSALS
        {
            return Err(GovernanceError::new_with_message(
                ErrorType::ResourceExhausted,
                "Reached maximum number of 'manage neuron' proposals. \
                Please try again later.",
            ));
        }
        // The electoral roll to put into the proposal.
        let electoral_roll: HashMap<u64, Ballot> = followees
            .followees
            .iter()
            .map(|x| {
                let vote = {
                    (if x.id == proposer_id.id {
                        Vote::Yes
                    } else {
                        Vote::Unspecified
                    }) as i32
                };
                (
                    x.id,
                    Ballot {
                        vote,
                        voting_power: 1,
                    },
                )
            })
            .collect();
        if electoral_roll.is_empty() {
            // Cannot make a proposal with no eligible voters.  This
            // is a precaution that shouldn't happen as we check that
            // the voter is allowed to vote.
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Empty electoral roll.",
            ));
        }
        // === Validation done.
        // Create a new proposal ID for this proposal.
        let proposal_num = self.next_proposal_id();
        let proposal_id = ProposalId { id: proposal_num };

        let title = Some(format!(
            "Manage neuron proposal for neuron: {}",
            managed_neuron
                .id
                .as_ref()
                .expect("Neurons must have an id")
                .id
        ));

        // Create the proposal.
        let info = ProposalData {
            id: Some(proposal_id),
            proposer: Some(proposer_id.clone()),
            proposal: Some(Proposal {
                title,
                summary: summary.to_string(),
                url: url.to_string(),
                action: Some(proposal::Action::ManageNeuron(Box::new(
                    manage_neuron.into_proto(),
                ))),
            }),
            proposal_timestamp_seconds: now_seconds,
            ballots: electoral_roll,
            ..Default::default()
        };

        // Charge fee.
        if let Some(proposer_mut) = self.proto.neurons.get_mut(&proposer_id.id) {
            proposer_mut.neuron_fees_e8s += neuron_management_fee_per_proposal_e8s
        }

        // Add this proposal as an open proposal.
        self.insert_proposal(proposal_num, info);

        Ok(proposal_id)
    }

    fn economics(&self) -> &NetworkEconomics {
        self.proto
            .economics
            .as_ref()
            .expect("NetworkEconomics not present")
    }

    /// Inserts a proposals that has already been validated in the state.
    ///
    /// This is a low-level function that makes no verification whatsoever.
    fn insert_proposal(&mut self, pid: u64, data: ProposalData) {
        let voting_period_seconds = self.voting_period_seconds()(data.topic());
        self.closest_proposal_deadline_timestamp_seconds = std::cmp::min(
            data.proposal_timestamp_seconds + voting_period_seconds,
            self.closest_proposal_deadline_timestamp_seconds,
        );
        self.proto.proposals.insert(pid, data);
        self.process_proposal(pid);
    }

    /// The proposal id of the next proposal.
    fn next_proposal_id(&self) -> u64 {
        // Correctness is based on the following observations:
        // * Proposal GC never removes the proposal with highest ID.
        // * The proposal map is a BTreeMap, so the proposals are ordered by id.
        self.proto
            .proposals
            .iter()
            .next_back()
            .map_or(1, |(k, _)| k + 1)
    }

    async fn validate_proposal(&mut self, proposal: &Proposal) -> Result<(), GovernanceError> {
        let invalid_proposal = |message| {
            Err(GovernanceError::new_with_message(
                ErrorType::InvalidProposal,
                message,
            ))
        };

        if proposal.topic() == Topic::Unspecified {
            return invalid_proposal("Topic not specified".to_string());
        }

        validate_proposal_title(&proposal.title)?;

        if !proposal.allowed_when_resources_are_low() {
            self.check_heap_can_grow()?;
        }

        if proposal.summary.len() > PROPOSAL_SUMMARY_BYTES_MAX {
            return invalid_proposal(format!(
                "The maximum proposal summary size is {} bytes, this proposal is: {} bytes",
                PROPOSAL_SUMMARY_BYTES_MAX,
                proposal.summary.len(),
            ));
        }

        // An empty string will fail validation as it is not a valid url,
        // but it's fine for us.
        if !proposal.url.is_empty() {
            validate_proposal_url(
                &proposal.url,
                PROPOSAL_URL_CHAR_MIN,
                PROPOSAL_URL_CHAR_MAX,
                "Proposal url",
                Some(vec!["forum.dfinity.org"]),
            )
            .map_err(|err| invalid_proposal(err).unwrap_err())?;
        }

        // Require that oneof action is populated.
        let action = proposal.action.as_ref().ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::InvalidProposal,
                format!("Proposal lacks an action: {:?}", proposal,),
            )
        })?;

        // Finally, perform Action-specific validation.
        match action {
            Action::ExecuteNnsFunction(execute_nns_function) => {
                self.validate_execute_nns_function(execute_nns_function)
            }

            Action::Motion(motion) => validate_motion(motion),

            Action::SetSnsTokenSwapOpenTimeWindow(set_sns_token_swap_open_time_window) => {
                validate_set_sns_token_swap_open_time_window(set_sns_token_swap_open_time_window)
            }

            Action::OpenSnsTokenSwap(open_sns_token_swap) => {
                self.validate_open_sns_token_swap(open_sns_token_swap).await
            }

            Action::ManageNeuron(_)
            | Action::ManageNetworkEconomics(_)
            | Action::ApproveGenesisKyc(_)
            | Action::AddOrRemoveNodeProvider(_)
            | Action::RewardNodeProvider(_)
            | Action::SetDefaultFollowees(_)
            | Action::RewardNodeProviders(_)
            | Action::RegisterKnownNeuron(_) => Ok(()),
        }
    }

    fn validate_execute_nns_function(
        &self,
        update: &ExecuteNnsFunction,
    ) -> Result<(), GovernanceError> {
        let error_str = {
            if update.nns_function != NnsFunction::NnsCanisterUpgrade as i32
                && update.nns_function != NnsFunction::NnsCanisterInstall as i32
                && update.nns_function != NnsFunction::NnsRootUpgrade as i32
                && update.nns_function != NnsFunction::AddSnsWasm as i32
                && update.payload.len() > PROPOSAL_EXECUTE_NNS_FUNCTION_PAYLOAD_BYTES_MAX
            {
                format!(
                    "The maximum NNS function payload size in a proposal action is {} bytes, this payload is: {} bytes",
                    PROPOSAL_EXECUTE_NNS_FUNCTION_PAYLOAD_BYTES_MAX,
                    update.payload.len(),
                )
            } else if update.nns_function == NnsFunction::IcpXdrConversionRate as i32 {
                match Decode!(&update.payload, UpdateIcpXdrConversionRatePayload) {
                    Ok(payload) => {
                        if payload.xdr_permyriad_per_icp
                            < self
                                .proto
                                .economics
                                .as_ref()
                                .ok_or_else(|| GovernanceError::new(ErrorType::Unavailable))?
                                .minimum_icp_xdr_rate
                        {
                            format!(
                                "The proposed rate {} is below the minimum allowable rate",
                                payload.xdr_permyriad_per_icp
                            )
                        } else {
                            return Ok(());
                        }
                    }
                    Err(e) => format!(
                        "The payload could not be decoded into a UpdateIcpXdrConversionRatePayload: {}",
                        e
                    ),
                }
            } else if update.nns_function == NnsFunction::AssignNoid as i32 {
                match Decode!(&update.payload, AddNodeOperatorPayload) {
                    Ok(payload) => match payload.node_provider_principal_id {
                        Some(id) => {
                            let is_registered = self
                                .get_node_providers()
                                .iter()
                                .any(|np| np.id.unwrap() == id);
                            if !is_registered {
                                "The node provider specified in the payload is not registered"
                                    .to_string()
                            } else {
                                return Ok(());
                            }
                        }
                        None => {
                            "The payload's node_provider_principal_id field was None".to_string()
                        }
                    },
                    Err(e) => format!(
                        "The payload could not be decoded into a AddNodeOperatorPayload: {}",
                        e
                    ),
                }
            } else if update.nns_function == NnsFunction::AddOrRemoveDataCenters as i32 {
                match Decode!(&update.payload, AddOrRemoveDataCentersProposalPayload) {
                    Ok(payload) => match payload.validate() {
                        Ok(_) => {
                            return Ok(());
                        }
                        Err(e) => {
                            format!("The given AddOrRemoveDataCentersProposalPayload is invalid: {}", e)
                        }
                    },
                    Err(e) => format!(
                        "The payload could not be decoded into a AddOrRemoveDataCentersProposalPayload: {}",
                        e
                    ),
                }
            } else {
                return Ok(());
            }
        };

        Err(GovernanceError::new_with_message(
            ErrorType::InvalidProposal,
            error_str,
        ))
    }

    /// There can be at most one OpenSnsTokenSwap proposal at a time.
    /// Of course, such proposals must be valid on their own as well.
    async fn validate_open_sns_token_swap(
        &mut self,
        open_sns_token_swap: &OpenSnsTokenSwap,
    ) -> Result<(), GovernanceError> {
        // Inspect open_sns_token_swap on its own.
        validate_open_sns_token_swap(open_sns_token_swap, &mut *self.env).await?;

        // Enforce that it would be unique.
        for proposal_data in self.proto.proposals.values() {
            if proposal_data.status() != ProposalStatus::Open {
                continue;
            }

            match &proposal_data.proposal {
                Some(Proposal {
                    action: Some(Action::OpenSnsTokenSwap(_)),
                    ..
                }) => {}
                _ => continue,
            }

            return Err(GovernanceError::new_with_message(
                ErrorType::InvalidProposal,
                format!(
                    "{}ERROR: there can only be at most one open OpenSnsTokenSwap proposal \
                     at a time, but there is already one: {:#?}",
                    LOG_PREFIX, proposal_data,
                ),
            ));
        }

        Ok(())
    }

    pub async fn make_proposal(
        &mut self,
        proposer_id: &NeuronId,
        caller: &PrincipalId,
        proposal: &Proposal,
    ) -> Result<ProposalId, GovernanceError> {
        let topic = proposal.topic();
        let now_seconds = self.env.now();

        // Validate proposal
        self.validate_proposal(proposal).await?;

        // Gather additional information for OpenSnsTokenSwap.
        let mut swap_background_information = None;
        if let Some(Action::OpenSnsTokenSwap(open_sns_token_swap)) = &proposal.action {
            swap_background_information = Some(
                // This makes some canister calls. In general, we have to be
                // careful, because if we call an untrusted canister, it might
                // never reply. Waiting for reply would block us from
                // upgrading. In this case, it's ok, because validate_proposal
                // has made sure that we are calling a trusted canister. One of
                // the things it does is consult the sns-wasm canister to make
                // sure we are talking to a known swap canister.
                fetch_swap_background_information(
                    &mut *self.env,
                    open_sns_token_swap
                        .target_swap_canister_id
                        .expect("target_swap_canister_id field empty.")
                        .try_into()
                        .unwrap_or_else(|err| {
                            panic!(
                                "Unable to convert target_swap_canister_id {:?} \
                                 into a CanisterId: {:?}",
                                open_sns_token_swap.target_swap_canister_id, err,
                            )
                        }),
                )
                .await?,
            );
        }

        if let Some(proposal::Action::ManageNeuron(m)) = &proposal.action {
            assert_eq!(topic, Topic::NeuronManagement);
            return self.make_manage_neuron_proposal(
                proposer_id,
                caller,
                now_seconds,
                m,
                &proposal.summary,
                &proposal.url,
            );
        }
        let reject_cost_e8s = self.economics().reject_cost_e8s;
        // Before actually modifying anything, we first make sure that
        // the neuron is allowed to make this proposal and create the
        // electoral roll.
        //
        // Find the proposing neuron.
        let proposer = self.proto.neurons.get(&proposer_id.id).ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::NotFound,
                format!("Proposer neuron not found: {}", proposer_id.id),
            )
        })?;
        // === Validation
        //
        // Check that the caller is authorized, i.e., either the
        // controller or a registered hot key.
        if !proposer.is_authorized_to_vote(caller) {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                "Caller not authorized to propose.",
            ));
        }
        // The proposer must be eligible to vote on its own
        // proposal. This also ensures that the neuron cannot be
        // dissolved until the proposal has been adopted or rejected.
        if proposer.dissolve_delay_seconds(now_seconds)
            < MIN_DISSOLVE_DELAY_FOR_VOTE_ELIGIBILITY_SECONDS
        {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Neuron's dissolve delay is too short.",
            ));
        }
        // If the current stake of this neuron is less than the cost
        // of having a proposal rejected, the neuron cannot vote -
        // because the proposal may be rejected.
        if proposer.minted_stake_e8s() < reject_cost_e8s {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!(
                    "Neuron doesn't have enough minted stake to submit proposal: {:#?}",
                    proposer,
                ),
            ));
        }
        // Check that there are not too many proposals.  What matters
        // here is the number of proposals for which ballots have not
        // yet been cleared, because ballots take the most amount of
        // space. (In the case of proposals with a wasm module in the
        // payload, the payload also takes a lot of space). Manage
        // neuron proposals are not counted as they have a smaller
        // electoral roll and use their own limit.
        if self
            .proto
            .proposals
            .values()
            .filter(|info| !info.ballots.is_empty() && !info.is_manage_neuron())
            .count()
            >= MAX_NUMBER_OF_PROPOSALS_WITH_BALLOTS
            && !proposal.allowed_when_resources_are_low()
        {
            return Err(GovernanceError::new_with_message(
                ErrorType::ResourceExhausted,
                "Reached maximum number of proposals that have not yet \
                been taken into account for voting rewards. \
                Please try again later.",
            ));
        }
        // === Preparation
        //
        // For normal proposals, every neuron with a
        // dissolve delay over six months is allowed to
        // vote, with a voting power determined at the
        // time of the proposal (i.e., now).
        //
        // The electoral roll to put into the proposal.
        assert!(
            !proposal.is_manage_neuron(),
            "{}Internal error: missing code to compute voting eligibility for a manage neuron \
             proposal with restricted voting. This code path is only for unrestricted proposals, and this function is \
             supposed to early-return for restricted proposals, but did not. \
             The offending proposal is: {:?}",
            LOG_PREFIX,
            proposal
        );
        let mut electoral_roll = HashMap::<u64, Ballot>::new();
        let mut total_power: u128 = 0;
        for (k, v) in self.proto.neurons.iter() {
            // If this neuron is eligible to vote, record its
            // voting power at the time of making the
            // proposal.
            if v.dissolve_delay_seconds(now_seconds)
                < MIN_DISSOLVE_DELAY_FOR_VOTE_ELIGIBILITY_SECONDS
            {
                // Not eligible due to dissolve delay.
                continue;
            }
            let power = v.voting_power(now_seconds);
            total_power += power as u128;
            electoral_roll.insert(
                *k,
                Ballot {
                    vote: Vote::Unspecified as i32,
                    voting_power: power,
                },
            );
        }
        if total_power >= (u64::MAX as u128) {
            // The way the neurons are configured, the total voting
            // power on this proposal would overflow a u64!
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Voting power overflow.",
            ));
        }
        if electoral_roll.is_empty() {
            // Cannot make a proposal with no eligible voters.  This
            // is a precaution that shouldn't happen as we check that
            // the voter is allowed to vote.
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "No eligible voters.",
            ));
        }
        // Create a new proposal ID for this proposal.
        let proposal_num = self.next_proposal_id();
        let proposal_id = ProposalId { id: proposal_num };
        let original_total_community_fund_maturity_e8s_equivalent =
            if let Some(Action::OpenSnsTokenSwap(_)) = proposal.action {
                Some(total_community_fund_maturity_e8s_equivalent(
                    &self.proto.neurons,
                ))
            } else {
                None
            };
        // Create the proposal.
        let derived_proposal_information = if swap_background_information.is_some() {
            Some(DerivedProposalInformation {
                swap_background_information,
            })
        } else {
            None
        };
        let mut info = ProposalData {
            id: Some(proposal_id),
            proposer: Some(proposer_id.clone()),
            reject_cost_e8s,
            proposal: Some(proposal.clone()),
            proposal_timestamp_seconds: now_seconds,
            ballots: electoral_roll,
            original_total_community_fund_maturity_e8s_equivalent,
            derived_proposal_information,
            ..Default::default()
        };

        info.wait_for_quiet_state = Some(WaitForQuietState {
            current_deadline_timestamp_seconds: now_seconds
                .saturating_add(self.voting_period_seconds()(topic)),
        });

        // Charge the cost of rejection upfront.
        // This will protect from DOS in couple of ways:
        // - It prevents a neuron from having too many proposals outstanding.
        // - It reduces the voting power of the submitter so that for every proposal
        //   outstanding the submitter will have less voting power to get it approved.
        self.proto
            .neurons
            .get_mut(&proposer_id.id)
            .expect("Proposer not found.")
            .neuron_fees_e8s += info.reject_cost_e8s;

        // Cast self-vote, including following.
        Governance::cast_vote_and_cascade_follow(
            &proposal_id,
            &mut info.ballots,
            proposer_id,
            Vote::Yes,
            topic,
            &self.topic_followee_index,
            &mut self.proto.neurons,
        );
        // Finally, add this proposal as an open proposal.
        self.insert_proposal(proposal_num, info);

        Ok(proposal_id)
    }

    // Register `voting_neuron_id` voting according to
    // `vote_of_neuron` (which must be `yes` or `no`) in 'ballots' and
    // cascade voting according to the following relationships
    // specified in 'followee_index' (mapping followees to followers for
    // the topic) and 'neurons' (which contains a mapping of followers
    // to followees).
    fn cast_vote_and_cascade_follow(
        proposal_id: &ProposalId,
        ballots: &mut HashMap<u64, Ballot>,
        voting_neuron_id: &NeuronId,
        vote_of_neuron: Vote,
        topic: Topic,
        topic_followee_index: &BTreeMap<Topic, BTreeMap<u64, BTreeSet<u64>>>,
        neurons: &mut HashMap<u64, Neuron>,
    ) {
        assert!(topic != Topic::NeuronManagement && topic != Topic::Unspecified);
        // This is the induction variable of the loop: a map from
        // neuron ID to the neuron's vote - 'yes' or 'no' (other
        // values not allowed).
        let mut induction_votes = BTreeMap::new();
        induction_votes.insert(voting_neuron_id.id, vote_of_neuron);
        let topic_cache = topic_followee_index.get(&topic);
        let unspecified_cache = topic_followee_index.get(&Topic::Unspecified);
        loop {
            // First, we cast the specified votes (in the first round,
            // this will be a single vote) and collect all neurons
            // that follow some of the neurons that are voting.
            let mut all_followers = BTreeSet::new();
            for (k, v) in induction_votes.iter() {
                // The new/induction votes cannot be unspecified.
                assert!(*v != Vote::Unspecified);
                if let Some(k_ballot) = ballots.get_mut(k) {
                    // Neuron with ID k is eligible to vote.
                    if k_ballot.vote == (Vote::Unspecified as i32) {
                        if let Some(k_neuron) = neurons.get_mut(k) {
                            // Only update a vote if it was previously
                            // unspecified. Following can trigger votes
                            // for neurons that have already voted
                            // (manually) and we don't change these votes.
                            k_ballot.vote = *v as i32;
                            // Register the neuron's ballot in the
                            // neuron itself.
                            k_neuron.register_recent_ballot(topic, proposal_id, *v);
                            // Here k is the followee, i.e., the neuron
                            // that has just cast a vote that may be
                            // followed by other neurons.
                            //
                            // Insert followers from 'topic'
                            if let Some(more_followers) = topic_cache.and_then(|x| x.get(k)) {
                                all_followers.append(&mut more_followers.clone());
                            }
                            // Default following doesn't apply to governance or SNS decentralization sale proposals.
                            if ![
                                Topic::Governance,
                                Topic::SnsDecentralizationSale,
                                Topic::SnsAndCommunityFund,
                            ]
                            .contains(&topic)
                            {
                                // Insert followers from 'Unspecified' (default followers)
                                if let Some(more_followers) =
                                    unspecified_cache.and_then(|x| x.get(k))
                                {
                                    all_followers.append(&mut more_followers.clone());
                                }
                            }
                        } else {
                            // The voting neuron not found in the
                            // neurons table. This is a bad
                            // inconsistency, but there is nothing
                            // that can be done about it at this
                            // place.
                        }
                    }
                } else {
                    // A non-eligible voter was specified in
                    // new/induction votes. We don't compute the
                    // followers of this neuron as it didn't actually
                    // vote.
                }
            }
            // Clear the induction_votes, as we are going to compute a
            // new set now.
            induction_votes.clear();
            for f in all_followers.iter() {
                if let Some(f_neuron) = neurons.get(f) {
                    let f_vote = f_neuron.would_follow_ballots(topic, ballots);
                    if f_vote != Vote::Unspecified {
                        // f_vote is yes or no, i.e., f_neuron's
                        // followee relations indicates that it should
                        // vote now.
                        induction_votes.insert(*f, f_vote);
                    }
                }
            }
            // If induction_votes is empty, the loop will terminate
            // here.
            if induction_votes.is_empty() {
                return;
            }
            // We now continue to the next iteration of the loop.
            // Because induction_votes is not empty, either at least
            // one entry in 'ballots' will change from unspecified to
            // yes or no, or all_followers will be empty, whence
            // induction_votes will become empty.
            //
            // Thus, for each iteration of the loop, the number of
            // entries in 'ballots' that have an unspecified value
            // decreases, or else the loop terminates. As nothing is
            // addded to 'ballots' (or removed for that matter), the
            // loop terminates in at most 'ballots.len()+1' steps.
            //
            // The worst case is attained if there is a linear
            // following graph, like this:
            //
            // X follows A follows B follows C,
            //
            // where X is not eligible to vote and nobody has
            // voted, i.e.,
            //
            // ballots = {
            //   A -> unspecified, B -> unspecified, C -> unspecified
            // }
            //
            // In this case, the subsequent values of
            // 'induction_votes' will be {C}, {B}, {A}, {X}.
            //
            // Note that it does not matter if X has followers. As X
            // doesn't vote, its followers are not considered.
            //
            // The above argument also shows how the algorithm deals
            // with cycles in the following graph: votes are
            // propagated through the graph in a manner similar to the
            // breadth-first search (BFS) algorithm. A node is
            // explored when it has voted yes or no.
        }
    }

    fn register_vote(
        &mut self,
        neuron_id: &NeuronId,
        caller: &PrincipalId,
        pb: &manage_neuron::RegisterVote,
    ) -> Result<(), GovernanceError> {
        let neuron = self.proto.neurons.get_mut(&neuron_id.id).ok_or_else(||
            // The specified neuron is not present.
            GovernanceError::new_with_message(ErrorType::NotFound, "Neuron not found"))?;
        // Check that the caller is authorized, i.e., either the
        // controller or a registered hot key.
        if !neuron.is_authorized_to_vote(caller) {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                "Caller is not authorized to vote for neuron.",
            ));
        }
        let proposal_id = pb.proposal.as_ref().ok_or_else(||
            // Proposal not specified.
            GovernanceError::new_with_message(ErrorType::PreconditionFailed, "Vote must include a proposal id."))?;
        let proposal = &mut (self.proto.proposals.get_mut(&proposal_id.id).ok_or_else(||
            // Proposal not found.
            GovernanceError::new_with_message(ErrorType::NotFound, "Can't find proposal."))?);
        let topic = proposal
            .proposal
            .as_ref()
            .map(|p| p.topic())
            .unwrap_or(Topic::Unspecified);
        let vote = Vote::from_i32(pb.vote).unwrap_or(Vote::Unspecified);
        if vote == Vote::Unspecified {
            // Invalid vote specified, i.e., not yes or no.
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Invalid vote specified.",
            ));
        }
        let mut neuron_ballot = proposal.ballots.get_mut(&neuron_id.id).ok_or_else(||
            // This neuron is not eligible to vote on this proposal.
            GovernanceError::new_with_message(ErrorType::NotAuthorized, "Neuron not authorized to vote on proposal."))?;
        if neuron_ballot.vote != (Vote::Unspecified as i32) {
            // Already voted.
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "Neuron already voted on proposal.",
            ));
        }
        if topic == Topic::NeuronManagement {
            // No following for manage neuron proposals.
            neuron_ballot.vote = vote as i32
        } else {
            Governance::cast_vote_and_cascade_follow(
                // Actually update the ballot, including following.
                proposal_id,
                &mut proposal.ballots,
                neuron_id,
                vote,
                topic,
                &self.topic_followee_index,
                &mut self.proto.neurons,
            );
        }

        self.process_proposal(proposal_id.id);

        Ok(())
    }

    /// Add or remove followees for this neuron for a specified topic.
    ///
    /// If the list of followees is empty, remove the followees for
    /// this topic. If the list has at least one element, replace the
    /// current list of followees for the given topic with the
    /// provided list. Note that the list is replaced, not addded to.
    fn follow(
        &mut self,
        id: &NeuronId,
        caller: &PrincipalId,
        f: &manage_neuron::Follow,
    ) -> Result<(), GovernanceError> {
        // The implementation of this method is complicated by the
        // fact that we have to maintain a reverse index of all follow
        // relationships, i.e., the `topic_followee_index`.

        // Find the neuron to modify.
        let neuron = self.proto.neurons.get_mut(&id.id).ok_or_else(||
            // The specified neuron is not present.
            GovernanceError::new_with_message(ErrorType::NotFound, format!("Leader neuron not found: {}", id.id)))?;

        // Only the controller, or a proposal (which passes the controller as the
        // caller), can change the followees for the ManageNeuron topic.
        if f.topic() == Topic::NeuronManagement && !neuron.is_controlled_by(caller) {
            return Err(GovernanceError::new_with_message(
                    ErrorType::NotAuthorized,
                    "Caller is not authorized to manage following of neuron for the ManageNeuron topic.",
                ));
        } else {
            // Check that the caller is authorized, i.e., either the
            // controller or a registered hot key.
            if !neuron.is_authorized_to_vote(caller) {
                return Err(GovernanceError::new_with_message(
                    ErrorType::NotAuthorized,
                    "Caller is not authorized to manage following of neuron.",
                ));
            }
        }

        // Check that the list of followees is not too
        // long. Allowing neurons to follow too many neurons
        // allows a memory exhaustion attack on the neurons
        // canister.
        if f.followees.len() > MAX_FOLLOWEES_PER_TOPIC {
            return Err(GovernanceError::new_with_message(
                ErrorType::InvalidCommand,
                "Too many followees.",
            ));
        }
        // First, remove the current followees for this neuron and
        // this topic from the follower cache.
        if let Some(neuron_followees) = neuron.followees.get(&f.topic) {
            if let Some(topic) = Topic::from_i32(f.topic) {
                // If this topic is not represented in the
                // follower cache, there cannot be anything to remove.
                if let Some(followee_index) = self.topic_followee_index.get_mut(&topic) {
                    // We need to remove this neuron as a follower
                    // for all followees.
                    for followee in &neuron_followees.followees {
                        if let Some(all_followers) = followee_index.get_mut(&followee.id) {
                            all_followers.remove(&id.id);
                        }
                        // Note: we don't check that the
                        // topic_followee_index actually contains this
                        // neuron's ID as a follower for all the
                        // followees. This could be a warning, but
                        // it is not actionable.
                    }
                }
            }
        }
        if !f.followees.is_empty() {
            // If this topic is valid, perform the operation.
            if let Some(topic) = Topic::from_i32(f.topic) {
                // Insert the new list of followees for this topic in
                // the neuron, removing the old list, which has
                // already been removed from the follower cache above.
                neuron.followees.insert(
                    f.topic,
                    Followees {
                        followees: f.followees.clone(),
                    },
                );
                let cache = self
                    .topic_followee_index
                    .entry(topic)
                    .or_insert_with(BTreeMap::new);
                // We need to to add this neuron as a follower for
                // all followees.
                for followee in &f.followees {
                    let all_followers = cache.entry(followee.id).or_insert_with(BTreeSet::new);
                    all_followers.insert(id.id);
                }
                Ok(())
            } else {
                // Attempt to follow for an invalid topic: the set
                // of followees for an invalid topic can be
                // removed, but not modified.
                Err(GovernanceError::new_with_message(
                    ErrorType::InvalidCommand,
                    "Invalid topic.",
                ))
            }
        } else {
            // This operation clears the followees for the given topic.
            neuron.followees.remove(&f.topic);
            Ok(())
        }
    }

    fn configure_neuron(
        &mut self,
        id: &NeuronId,
        caller: &PrincipalId,
        c: &manage_neuron::Configure,
    ) -> Result<(), GovernanceError> {
        let now_seconds = self.env.now();

        let lock_command = NeuronInFlightCommand {
            timestamp: now_seconds,
            command: Some(InFlightCommand::Configure(c.clone())),
        };
        let _lock = self.lock_neuron_for_command(id.id, lock_command)?;

        if let Some(neuron) = self.proto.neurons.get_mut(&id.id) {
            neuron.configure(caller, now_seconds, c)?;

            let op = c
                .operation
                .as_ref()
                .expect("Configure must have an operation");

            match op {
                manage_neuron::configure::Operation::AddHotKey(k) => {
                    let hot_key = k.new_hot_key.as_ref().expect("Must have a hot key");
                    GovernanceProto::add_neuron_to_principal_in_principal_to_neuron_ids_index(
                        &mut self.principal_to_neuron_ids_index,
                        id.id,
                        hot_key,
                    );
                }
                manage_neuron::configure::Operation::RemoveHotKey(k) => {
                    let hot_key = k.hot_key_to_remove.as_ref().expect("Must have a hot key");
                    if neuron.controller.as_ref() != Some(hot_key) {
                        GovernanceProto::remove_neuron_from_principal_in_principal_to_neuron_ids_index(
                            &mut self.principal_to_neuron_ids_index,
                            neuron,
                            hot_key,
                        );
                    }
                }
                _ => (),
            }
            Ok(())
        } else {
            Err(GovernanceError::new_with_message(
                ErrorType::NotFound,
                "Neuron not found.",
            ))
        }
    }

    /// Creates a new neuron or refreshes the stake of an existing
    /// neuron from a ledger account.
    ///
    /// Pre-conditions:
    /// - The memo must match the nonce of the subaccount.
    ///
    /// Post-conditions:
    /// - If all the pre-conditions apply, either a new neuron is created or the
    ///   stake of an existing neuron is updated.
    async fn claim_or_refresh_neuron_by_memo_and_controller(
        &mut self,
        caller: &PrincipalId,
        memo_and_controller: MemoAndController,
        claim_or_refresh: &ClaimOrRefresh,
    ) -> Result<NeuronId, GovernanceError> {
        let controller = memo_and_controller.controller.unwrap_or(*caller);
        let memo = memo_and_controller.memo;
        let subaccount = ledger::compute_neuron_staking_subaccount(controller, memo);
        match self.get_neuron_by_subaccount(&subaccount) {
            Some(neuron) => {
                let nid = neuron.id.as_ref().expect("Neuron must have an id").clone();
                self.refresh_neuron(nid, subaccount, claim_or_refresh).await
            }
            None => {
                self.claim_neuron(subaccount, controller, claim_or_refresh)
                    .await
            }
        }
    }

    /// Refreshes the neuron, getting both it's id and subaccount, if only one
    /// of them was provided as argument.
    async fn refresh_neuron_by_id_or_subaccount(
        &mut self,
        id: NeuronIdOrSubaccount,
        claim_or_refresh: &ClaimOrRefresh,
    ) -> Result<NeuronId, GovernanceError> {
        let (nid, subaccount) = match id {
            NeuronIdOrSubaccount::NeuronId(nid) => {
                let neuron = self.get_neuron(&nid)?;
                let subaccount = Self::bytes_to_subaccount(&neuron.account)?;
                (nid, subaccount)
            }
            NeuronIdOrSubaccount::Subaccount(sid) => {
                let subaccount = Self::bytes_to_subaccount(&sid)?;
                let neuron = self
                    .get_neuron_by_subaccount(&subaccount)
                    .ok_or_else(|| Self::no_neuron_for_subaccount_error(&sid))?;
                (
                    neuron.id.as_ref().expect("Neurons must have an id").clone(),
                    subaccount,
                )
            }
        };
        self.refresh_neuron(nid, subaccount, claim_or_refresh).await
    }

    /// Refreshes the stake of a given neuron by checking it's account.
    async fn refresh_neuron(
        &mut self,
        nid: NeuronId,
        subaccount: Subaccount,
        claim_or_refresh: &ClaimOrRefresh,
    ) -> Result<NeuronId, GovernanceError> {
        let account = neuron_subaccount(subaccount);
        // We need to lock the neuron to make sure it doesn't undergo
        // concurrent changes while we're checking the balance and
        // refreshing the stake.
        let now = self.env.now();
        let _neuron_lock = self.lock_neuron_for_command(
            nid.id,
            NeuronInFlightCommand {
                timestamp: now,
                command: Some(InFlightCommand::ClaimOrRefreshNeuron(
                    claim_or_refresh.clone(),
                )),
            },
        )?;

        // Get the balance of the neuron from the ledger canister.
        let balance = self.ledger.account_balance(account).await?;
        let min_stake = self.economics().neuron_minimum_stake_e8s;
        if balance.get_e8s() < min_stake {
            return Err(GovernanceError::new_with_message(
                ErrorType::InsufficientFunds,
                format!(
                    "Account does not have enough funds to refresh a neuron. \
                     Please make sure that account has at least {:?} e8s (was {:?} e8s)",
                    min_stake,
                    balance.get_e8s()
                ),
            ));
        }
        let neuron = self.get_neuron_mut(&nid)?;
        match neuron.cached_neuron_stake_e8s.cmp(&balance.get_e8s()) {
            Ordering::Greater => {
                println!(
                    "{}ERROR. Neuron cached stake was inconsistent.\
                     Neuron account: {} has less e8s: {} than the cached neuron stake: {}.\
                     Stake adjusted.",
                    LOG_PREFIX,
                    account,
                    balance.get_e8s(),
                    neuron.cached_neuron_stake_e8s
                );
                neuron.update_stake(balance.get_e8s(), now);
            }
            Ordering::Less => {
                neuron.update_stake(balance.get_e8s(), now);
            }
            // If the stake is the same as the account balance,
            // just return the neuron id (this way this method
            // also serves the purpose of allowing to discover the
            // neuron id based on the memo and the controller).
            Ordering::Equal => (),
        };

        Ok(nid)
    }

    /// Claim a new neuron, unless the account doesn't have enough to stake a
    /// neuron or we've reached the maximum number of neurons, in which case
    /// we return an error.
    ///
    /// We can't return the funds without more information about the
    /// source account, so as a workaround for insufficient stake we can ask the
    /// user to transfer however much is missing to stake a neuron and they can
    /// then disburse if they so choose. We need to do something more involved
    /// if we've reached the max, TODO.
    ///
    /// Preconditions:
    /// - The new neuron won't take us above the `MAX_NUMBER_OF_NEURONS`.
    /// - The amount transfered was greater than or equal to
    ///   `self.enconomics.neuron_minimum_stake_e8s`.
    ///
    /// Note that we need to create the neuron before checking the balance
    /// so that we record the neuron and avoid a race where a user calls
    /// this method a second time before the first time responds. If we store
    /// the neuron and lock it before we make the call, we know that any
    /// concurrent call to mutate the same neuron will need to wait for this
    /// one to finish before proceeding.
    async fn claim_neuron(
        &mut self,
        subaccount: Subaccount,
        controller: PrincipalId,
        claim_or_refresh: &ClaimOrRefresh,
    ) -> Result<NeuronId, GovernanceError> {
        let nid = self.new_neuron_id();
        let now = self.env.now();
        let neuron = Neuron {
            id: Some(nid.clone()),
            account: subaccount.to_vec(),
            controller: Some(controller),
            cached_neuron_stake_e8s: 0,
            created_timestamp_seconds: now,
            aging_since_timestamp_seconds: now,
            dissolve_state: Some(DissolveState::DissolveDelaySeconds(0)),
            transfer: None,
            kyc_verified: true,
            followees: self.proto.default_followees.clone(),
            hot_keys: vec![],
            maturity_e8s_equivalent: 0,
            staked_maturity_e8s_equivalent: None,
            auto_stake_maturity: None,
            neuron_fees_e8s: 0,
            not_for_profit: false,
            recent_ballots: vec![],
            joined_community_fund_timestamp_seconds: None,
            known_neuron_data: None,
            spawn_at_timestamp_seconds: None,
        };

        // This also verifies that there are not too many neurons already.
        self.add_neuron(nid.id, neuron.clone())?;

        let _neuron_lock = self.lock_neuron_for_command(
            nid.id,
            NeuronInFlightCommand {
                timestamp: now,
                command: Some(InFlightCommand::ClaimOrRefreshNeuron(
                    claim_or_refresh.clone(),
                )),
            },
        )?;

        // Get the balance of the neuron's subaccount from ledger canister.
        let account = neuron_subaccount(subaccount);
        let balance = self.ledger.account_balance(account).await?;
        let min_stake = self.economics().neuron_minimum_stake_e8s;
        if balance.get_e8s() < min_stake {
            // To prevent this method from creating non-staked
            // neurons, we must also remove the neuron that was
            // previously created.
            self.remove_neuron(nid.id, neuron)?;
            return Err(GovernanceError::new_with_message(
                ErrorType::InsufficientFunds,
                format!(
                    "Account does not have enough funds to stake a neuron. \
                     Please make sure that account has at least {:?} e8s (was {:?} e8s)",
                    min_stake,
                    balance.get_e8s()
                ),
            ));
        }

        // Ok, we are able to stake the neuron.
        match self.get_neuron_mut(&nid) {
            Ok(neuron) => {
                // Adjust the stake.
                neuron.update_stake(balance.get_e8s(), now);
                Ok(nid)
            }
            Err(err) => {
                // This should not be possible, but let's be defensive and provide a
                // reasonable error message, but still panic so that the lock remains
                // acquired and we can investigate.
                panic!(
                    "When attempting to stake a neuron with ID {:?} and stake {:?},\
                    the neuron disappeared while the operation was in flight.\
                    Please try again: {:?}",
                    nid,
                    balance.get_e8s(),
                    err
                )
            }
        }
    }

    /// Add some identifying metadata to a neuron. This metadata is represented
    /// in KnownNeuronData and includes:
    ///  - Name: the name given to the neuron.
    ///  - Description: optional field to add a short description of the neuron,
    ///    or organization behind it.
    ///
    /// Preconditions:
    ///  - A Neuron ID is given in the request and this ID identifies an existing neuron.
    ///  - Known Neuron Data is specified in the request.
    ///  - Name is at most of length KNOWN_NEURON_NAME_MAX_LEN.
    ///  - Description, if present, is at most of length KNOWN_NEURON_DESCRIPTION_MAX_LEN.
    ///  - Name is not already used in another known neuron.
    fn register_known_neuron(&mut self, known_neuron: KnownNeuron) -> Result<(), GovernanceError> {
        let neuron_id = known_neuron.id.ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::NotFound,
                "No neuron ID specified in the request to register a known neuron.",
            )
        })?;
        let known_neuron_data = known_neuron.known_neuron_data.as_ref().ok_or_else(|| {
            GovernanceError::new_with_message(
                ErrorType::NotFound,
                "No known neuron data specified in the register neuron request.",
            )
        })?;
        if known_neuron_data.name.len() > KNOWN_NEURON_NAME_MAX_LEN {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                format!(
                    "The maximum length for a neuron's name, which is {}, has been exceeded",
                    KNOWN_NEURON_NAME_MAX_LEN
                ),
            ));
        }
        if known_neuron_data.description.is_some()
            && known_neuron_data.description.as_ref().unwrap().len()
                > KNOWN_NEURON_DESCRIPTION_MAX_LEN
        {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                format!(
                    "The maximum length for a neuron's description, which is {}, has been exceeded",
                    KNOWN_NEURON_DESCRIPTION_MAX_LEN
                ),
            ));
        }
        if self.known_neuron_name_set.contains(&known_neuron_data.name) {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                format!(
                    "The name {} already belongs to a Neuron",
                    known_neuron_data.name
                ),
            ));
        }

        let neuron = self.proto.neurons.get_mut(&neuron_id.id).ok_or_else(||
            // The specified neuron is not present.
            GovernanceError::new_with_message(ErrorType::NotFound, "Neuron not found"))?;
        if let Some(KnownNeuronData { name: old_name, .. }) = &neuron.known_neuron_data {
            self.known_neuron_name_set.remove(old_name);
        }
        neuron.known_neuron_data = Some(known_neuron_data.clone());
        self.known_neuron_name_set
            .insert(known_neuron_data.name.clone());

        Ok(())
    }

    pub async fn manage_neuron(
        &mut self,
        caller: &PrincipalId,
        mgmt: &ManageNeuron,
    ) -> ManageNeuronResponse {
        self.manage_neuron_internal(caller, mgmt)
            .await
            .unwrap_or_else(ManageNeuronResponse::error)
    }

    pub async fn manage_neuron_internal(
        &mut self,
        caller: &PrincipalId,
        mgmt: &ManageNeuron,
    ) -> Result<ManageNeuronResponse, GovernanceError> {
        // We run claim or refresh before we check whether a neuron exists because it
        // may not in the case of the neuron being claimed
        if let Some(manage_neuron::Command::ClaimOrRefresh(claim_or_refresh)) = &mgmt.command {
            // Note that we return here, so none of the rest of this method is executed
            // in this case.
            return match &claim_or_refresh.by {
                Some(By::Memo(memo)) => {
                    let memo_and_controller = MemoAndController {
                        memo: *memo,
                        controller: None,
                    };
                    self.claim_or_refresh_neuron_by_memo_and_controller(
                        caller,
                        memo_and_controller,
                        claim_or_refresh,
                    )
                    .await
                    .map(ManageNeuronResponse::claim_or_refresh_neuron_response)
                }
                Some(By::MemoAndController(memo_and_controller)) => self
                    .claim_or_refresh_neuron_by_memo_and_controller(
                        caller,
                        memo_and_controller.clone(),
                        claim_or_refresh,
                    )
                    .await
                    .map(ManageNeuronResponse::claim_or_refresh_neuron_response),

                Some(By::NeuronIdOrSubaccount(_)) => {
                    let id = mgmt.get_neuron_id_or_subaccount()?.ok_or_else(|| {
                        GovernanceError::new_with_message(
                            ErrorType::NotFound,
                            "No neuron ID specified in the management request.",
                        )
                    })?;
                    self.refresh_neuron_by_id_or_subaccount(id, claim_or_refresh)
                        .await
                        .map(ManageNeuronResponse::claim_or_refresh_neuron_response)
                }
                None => Err(GovernanceError::new_with_message(
                    ErrorType::InvalidCommand,
                    "Need to provide a way by which to claim or refresh the neuron.",
                )),
            };
        }

        let id = match mgmt.get_neuron_id_or_subaccount()? {
            Some(NeuronIdOrSubaccount::NeuronId(id)) => Ok(id),
            Some(NeuronIdOrSubaccount::Subaccount(sid)) => {
                let subaccount = Self::bytes_to_subaccount(&sid)?;
                match self.get_neuron_by_subaccount(&subaccount) {
                    Some(neuron) => Ok(neuron.id.clone().expect("neuron doesn't have an ID")),
                    None => Err(GovernanceError::new_with_message(
                        ErrorType::NotFound,
                        "No neuron ID specified in the management request.",
                    )),
                }
            }
            None => Err(GovernanceError::new_with_message(
                ErrorType::NotFound,
                "No neuron ID specified in the management request.",
            )),
        }?;

        match &mgmt.command {
            Some(manage_neuron::Command::Configure(c)) => self
                .configure_neuron(&id, caller, c)
                .map(|_| ManageNeuronResponse::configure_response()),
            Some(manage_neuron::Command::Disburse(d)) => self
                .disburse_neuron(&id, caller, d)
                .await
                .map(ManageNeuronResponse::disburse_response),
            Some(manage_neuron::Command::Spawn(s)) => self
                .spawn_neuron(&id, caller, s)
                .await
                .map(ManageNeuronResponse::spawn_response),
            Some(manage_neuron::Command::MergeMaturity(m)) => self
                .redirect_merge_maturity_to_stake_maturity(&id, caller, m)
                .map(ManageNeuronResponse::merge_maturity_response),
            Some(manage_neuron::Command::StakeMaturity(s)) => self
                .stake_maturity_of_neuron(&id, caller, s)
                .map(|(response, _)| ManageNeuronResponse::stake_maturity_response(response)),
            Some(manage_neuron::Command::Split(s)) => self
                .split_neuron(&id, caller, s)
                .await
                .map(ManageNeuronResponse::split_response),
            Some(manage_neuron::Command::DisburseToNeuron(d)) => self
                .disburse_to_neuron(&id, caller, d)
                .await
                .map(ManageNeuronResponse::disburse_to_neuron_response),
            Some(manage_neuron::Command::Merge(s)) => self
                .merge_neurons(&id, caller, s)
                .await
                .map(|_| ManageNeuronResponse::merge_response()),
            Some(manage_neuron::Command::Follow(f)) => self
                .follow(&id, caller, f)
                .map(|_| ManageNeuronResponse::follow_response()),
            Some(manage_neuron::Command::MakeProposal(p)) => self
                .make_proposal(&id, caller, p)
                .await
                .map(ManageNeuronResponse::make_proposal_response),
            Some(manage_neuron::Command::RegisterVote(v)) => self
                .register_vote(&id, caller, v)
                .map(|_| ManageNeuronResponse::register_vote_response()),
            Some(manage_neuron::Command::ClaimOrRefresh(_)) => {
                panic!("This should have already returned")
            }
            None => panic!(),
        }
    }

    /// Garbage collect obsolete data from the governance canister.
    ///
    /// Current implementation only garbage collects proposals - not neurons.
    ///
    /// Returns true if GC was run and false otherwise.
    pub fn maybe_gc(&mut self) -> bool {
        let now_seconds = self.env.now();
        // Run GC if either (a) more than 24 hours has passed since it
        // was run last, or (b) more than 100 proposals have been
        // added since it was run last.
        if !(now_seconds > self.latest_gc_timestamp_seconds + 60 * 60 * 24
            || self.proto.proposals.len() > self.latest_gc_num_proposals + 100)
        {
            // Condition to run was not met. Return false.
            return false;
        }
        self.latest_gc_timestamp_seconds = self.env.now();
        println!(
            "{}Running GC now at timestamp {} seconds",
            LOG_PREFIX, self.latest_gc_timestamp_seconds
        );
        let max_proposals = self.economics().max_proposals_to_keep_per_topic as usize;
        // If `max_proposals_to_keep_per_topic` is unspecified, or
        // specified as zero, don't garbage collect any proposals.
        if max_proposals == 0 {
            return true;
        }
        // This data structure contains proposals grouped by topic.
        let proposals_by_topic = {
            let mut tmp: HashMap<Topic, Vec<u64>> = HashMap::new();
            for (id, prop) in self.proto.proposals.iter() {
                tmp.entry(prop.topic()).or_insert_with(Vec::new).push(*id);
            }
            tmp
        };
        // Only keep the latest 'max_proposals' per topic.
        for (topic, props) in proposals_by_topic {
            let voting_period_seconds = self.voting_period_seconds()(topic);
            println!(
                "{}GC - topic {:#?} max {} current {}",
                LOG_PREFIX,
                topic,
                max_proposals,
                props.len()
            );
            if props.len() > max_proposals {
                for prop_id in props.iter().take(props.len() - max_proposals) {
                    // Check that this proposal can be purged.
                    if let Some(prop) = self.proto.proposals.get(prop_id) {
                        if prop.can_be_purged(now_seconds, voting_period_seconds) {
                            self.proto.proposals.remove(prop_id);
                        }
                    }
                }
            }
        }
        self.latest_gc_num_proposals = self.proto.proposals.len();
        true
    }

    /// Triggers a reward distribution event if enough time has passed since
    /// the last one. This is intended to be called by a cron
    /// process.
    pub async fn run_periodic_tasks(&mut self) {
        self.process_proposals();

        // First try to mint node provider rewards (once per month).
        if self.is_time_to_mint_monthly_node_provider_rewards() {
            match self.mint_monthly_node_provider_rewards().await {
                Ok(()) => (),
                Err(e) => println!(
                    "{}Error when minting monthly node provider rewards in run_periodic_tasks: {}",
                    LOG_PREFIX, e,
                ),
            }
        // Second try to distribute voting rewards (once per day).
        } else if self.should_distribute_rewards() {
            // Getting the total ICP supply from the ledger is expensive enough that we
            // don't want to do it on every call to `run_periodic_tasks`. So we only
            // fetch it when it's needed.
            match self.ledger.total_supply().await {
                Ok(supply) => {
                    if self.should_distribute_rewards() {
                        self.distribute_rewards(supply);
                    }
                }
                Err(e) => println!(
                    "{}Error when getting total ICP supply: {}",
                    LOG_PREFIX,
                    GovernanceError::from(e),
                ),
            }
        // Third try to compute cached metrics (once per day).
        } else if self.should_compute_cached_metrics() {
            match self.ledger.total_supply().await {
                Ok(supply) => {
                    if self.should_compute_cached_metrics() {
                        let now = self.env.now();
                        let metrics = self.proto.compute_cached_metrics(now, supply);
                        self.proto.metrics = Some(metrics);
                    }
                }
                Err(e) => println!(
                    "{}Error when getting total ICP supply: {}",
                    LOG_PREFIX,
                    GovernanceError::from(e),
                ),
            }
        // Try to update maturity modulation (once per day).
        } else if self.should_update_maturity_modulation() {
            self.update_maturity_modulation().await;
        // Try to spawn neurons (potentially multiple times per day).
        } else if self.can_spawn_neurons() {
            self.spawn_neurons().await;
        }

        self.maybe_move_staked_maturity();
        self.maybe_gc();
    }

    fn should_update_maturity_modulation(&self) -> bool {
        // Check if we're already updating the neuron maturity modulation.
        let now_seconds = self.env.now();
        let last_updated = self
            .proto
            .maturity_modulation_last_updated_at_timestamp_seconds;
        last_updated.is_none() || last_updated.unwrap() + ONE_DAY_SECONDS <= now_seconds
    }

    async fn update_maturity_modulation(&mut self) {
        if !self.should_update_maturity_modulation() {
            return;
        };

        let now_seconds = self.env.now();
        let maturity_modulation = self.cmc.neuron_maturity_modulation().await;
        if maturity_modulation.is_err() {
            println!(
                "{}Couldn't update maturity modulation. Error: {}",
                LOG_PREFIX,
                maturity_modulation.err().unwrap()
            );
            return;
        }
        let maturity_modulation = maturity_modulation.unwrap();
        println!(
            "{}Updated daily maturity modulation rate to (in basis points): {}, at: {}. Last updated: {:?}",
            LOG_PREFIX, maturity_modulation, now_seconds, self.proto.maturity_modulation_last_updated_at_timestamp_seconds,
        );
        self.proto.cached_daily_maturity_modulation_basis_points = Some(maturity_modulation);
        self.proto
            .maturity_modulation_last_updated_at_timestamp_seconds = Some(now_seconds);
    }

    /// When a neuron is finally dissolved, if there is any staked maturity it is moved to regular maturity
    /// which can be spawned (and is modulated).
    fn maybe_move_staked_maturity(&mut self) {
        let now_seconds = self.env.now();
        // Filter all the neurons that are currently in "dissolved" state and have some staked maturity.
        for neuron in self.proto.neurons.values_mut().filter(|n| {
            n.state(now_seconds) == NeuronState::Dissolved
                && n.staked_maturity_e8s_equivalent.unwrap_or(0) > 0
        }) {
            neuron.maturity_e8s_equivalent = neuron
                .maturity_e8s_equivalent
                .saturating_add(neuron.staked_maturity_e8s_equivalent.unwrap_or(0));
            neuron.staked_maturity_e8s_equivalent = None;
        }
    }

    fn can_spawn_neurons(&self) -> bool {
        let spawning = self.proto.spawning_neurons;
        spawning.is_none() || !spawning.unwrap()
    }

    /// Actually spawn neurons by minting their maturity, modulated by the maturity modulation rate of the day.
    /// There can only be one execution of this method running at a time to keep the reasoning about this simple.
    /// This means that programming in this method needs to be extra-defensive on the handling of results so that
    /// we're sure not to trap after we've acquired the global lock and made an async call, as otherwise the global
    /// lock will be permanently held and no spawning will occur until a upgrade to fix it is made.
    async fn spawn_neurons(&mut self) {
        if !self.can_spawn_neurons() {
            return;
        }

        let now_seconds = self.env.now();
        let maturity_modulation = match self.proto.cached_daily_maturity_modulation_basis_points {
            None => return,
            Some(value) => value,
        };

        // Sanity check that the maturity modulation returned is within bounds.
        if !VALID_MATURITY_MODULATION_BASIS_POINTS_RANGE.contains(&maturity_modulation) {
            println!(
                "{}Maturity modulation (in basis points) out-of-bounds. Should be in range [-500, 500], actually is: {}",
                LOG_PREFIX, maturity_modulation
            );
            return;
        }

        // Acquire the global "spawning" lock.
        self.proto.spawning_neurons = Some(true);

        // Filter all the neurons that are currently in "spawning" state.
        // Do this here to avoid having to borrow *self while we perform changes below.
        let spawning_neurons = self
            .proto
            .neurons
            .values()
            .cloned()
            .filter(|n| n.state(now_seconds) == NeuronState::Spawning)
            .collect::<Vec<Neuron>>();

        for neuron in spawning_neurons {
            let spawn_timestamp_seconds = neuron
                .spawn_at_timestamp_seconds
                .expect("Neuron is spawning but has no spawn timestamp");

            if now_seconds >= spawn_timestamp_seconds {
                let id = neuron.id.as_ref().unwrap().clone();
                let subaccount = neuron.account.clone();
                // Actually mint the neuron's ICP.
                let in_flight_command = NeuronInFlightCommand {
                    timestamp: now_seconds,
                    command: Some(InFlightCommand::Spawn(neuron.id.as_ref().unwrap().clone())),
                };

                // Add the neuron to the set of neurons undergoing ledger updates.
                match self.lock_neuron_for_command(id.id, in_flight_command.clone()) {
                    Ok(mut lock) => {
                        // Since we're multiplying a potentially pretty big number by up to 10500, do
                        // the calculations as u128 before converting back.
                        let maturity = neuron.maturity_e8s_equivalent as u128;
                        let neuron_stake: u64 = maturity
                            .checked_mul((10000 + maturity_modulation).try_into().unwrap())
                            .unwrap()
                            .checked_div(10000)
                            .unwrap()
                            .try_into()
                            .expect("Couldn't convert stake to u64");

                        println!(
                            "{}Spawning neuron: {:?}. Performing ledger udpate.",
                            LOG_PREFIX, neuron
                        );

                        let mut neuron = self.get_neuron_mut(&id).unwrap();

                        // Reset the neuron's maturity and set that it's spawning before we actually mint
                        // the stake. This is conservative to prevent a neuron having _both_ the stake and
                        // the maturity at any point in time.
                        neuron.maturity_e8s_equivalent = 0;
                        neuron.spawn_at_timestamp_seconds = None;
                        neuron.cached_neuron_stake_e8s = neuron_stake;

                        let neuron_clone = neuron.clone();

                        // Do the transfer, this is a minting transfer, from the governance canister's
                        // (which is also the minting canister) main account into the neuron's
                        // subaccount.
                        match self
                            .ledger
                            .transfer_funds(
                                neuron_stake,
                                0, // Minting transfer don't pay a fee.
                                None,
                                neuron_subaccount(
                                    Subaccount::try_from(&subaccount[..])
                                        .expect("Couldn't convert neuron.account"),
                                ),
                                now_seconds,
                            )
                            .await
                        {
                            Ok(_) => {
                                println!(
                                    "{}Spawned neuron: {:?}. Ledger update performed.",
                                    LOG_PREFIX, neuron_clone,
                                );
                            }
                            Err(error) => {
                                // Retain the neuron lock, the neuron won't be able to undergo stake changing
                                // operations until this is fixed.
                                // This is different from what we do in most places because we usually rely
                                // on trapping to retain the lock, but we can't do that here since we're not
                                // working on a single neuron.
                                lock.retain();
                                println!(
                                    "{}Error spawning neuron: {:?}. Ledger update failed with err: {:?}.",
                                    LOG_PREFIX,
                                    id,
                                    error,
                                );
                            }
                        };
                    }
                    Err(error) => {
                        // If the lock was already acquired, just continue.
                        println!(
                            "{}Tried to spawn neuron but was already locked: {:?}. Error: {:?}",
                            LOG_PREFIX, id, error,
                        );
                        continue;
                    }
                }
            }
        }

        // Release the global spawning lock
        self.proto.spawning_neurons = Some(false);
    }

    /// Return `true` if rewards should be distributed, `false` otherwise
    fn should_distribute_rewards(&self) -> bool {
        let reward_available_at = self.proto.genesis_timestamp_seconds
            + (self.latest_reward_event().day_after_genesis + 1)
                * REWARD_DISTRIBUTION_PERIOD_SECONDS;

        self.env.now() >= reward_available_at
    }

    /// Create a reward event.
    ///
    /// This method:
    /// * collects all proposals in state ReadyToSettle, that is, proposals that
    /// can no longer accept votes for the purpose of rewards and that have
    /// not yet been considered in a reward event.
    /// * Associate those proposals to the new reward event
    fn distribute_rewards(&mut self, supply: Tokens) {
        println!("{}distribute_rewards. Supply: {:?}", LOG_PREFIX, supply);

        let day_after_genesis = (self.env.now() - self.proto.genesis_timestamp_seconds)
            / REWARD_DISTRIBUTION_PERIOD_SECONDS;

        if day_after_genesis <= self.latest_reward_event().day_after_genesis {
            // This may happen, in case consider_distributing_rewards was called
            // several times at almost the same time. This is
            // harmless, just abandon.
            return;
        }

        if day_after_genesis > 1 + self.latest_reward_event().day_after_genesis {
            println!(
                "{}Some reward distribution should have happened, but were missed.\
                      It is now {} full days since IC genesis, and the last distribution\
                      nominally happened at {} full days since IC genesis.",
                LOG_PREFIX,
                day_after_genesis,
                self.latest_reward_event().day_after_genesis
            );
        }
        let days = self.latest_reward_event().day_after_genesis..day_after_genesis;
        let fraction: f64 = days
            .map(crate::reward::rewards_pool_to_distribute_in_supply_fraction_for_one_day)
            .sum();

        let distributed_e8s_equivalent_float = (supply.get_e8s() as f64) * fraction;
        // We should not convert right away to integer! The
        // "distributed_e8s_equivalent" recorded in the RewardEvent proto
        // should match exactly the sum of the distributed integer e8
        // equivalents. Due to rounding, we actually need to recompute this sum,
        // even though it will be very close to distributed_e8s_equivalent_float.
        let mut actually_distributed_e8s_equivalent = 0_u64;

        let considered_proposals: Vec<ProposalId> =
            self.ready_to_be_settled_proposal_ids().collect();

        // Construct map voters -> total _used_ voting rights for considered proposals
        let (voters_to_used_voting_right, total_voting_rights) = {
            let mut voters_to_used_voting_right: HashMap<NeuronId, f64> = HashMap::new();
            let mut total_voting_rights = 0f64;

            for pid in considered_proposals.iter() {
                if let Some(proposal) = self.get_proposal_data(*pid) {
                    let reward_weight = proposal.topic().reward_weight();
                    for (voter, ballot) in proposal.ballots.iter() {
                        if !Vote::from(ballot.vote).eligible_for_rewards() {
                            continue;
                        }
                        let voting_rights = (ballot.voting_power as f64) * reward_weight;
                        *voters_to_used_voting_right
                            .entry(NeuronId { id: *voter })
                            .or_insert(0f64) += voting_rights;
                        total_voting_rights += voting_rights;
                    }
                }
            }
            (voters_to_used_voting_right, total_voting_rights)
        };

        for (neuron_id, used_voting_rights) in voters_to_used_voting_right {
            match self.get_neuron_mut(&neuron_id) {
                Ok(mut neuron) => {
                    // Note that "as" rounds toward zero; this is the desired
                    // behavior here. Also note that `total_voting_rights` has
                    // to be positive because (1) voters_to_used_voting_right
                    // is non-empty (otherwise we wouldn't be here in the
                    // first place) and (2) the voting power of all ballots is
                    // positive (non-zero).
                    let reward = (used_voting_rights * distributed_e8s_equivalent_float
                        / total_voting_rights) as u64;
                    // If the neuron has auto-stake-maturity on, add the new maturity to the
                    // staked maturity, otherwise add it to the un-staked maturity.
                    if neuron.auto_stake_maturity.unwrap_or(false) {
                        neuron.staked_maturity_e8s_equivalent =
                            Some(neuron.staked_maturity_e8s_equivalent.unwrap_or(0) + reward);
                    } else {
                        neuron.maturity_e8s_equivalent += reward;
                    }
                    actually_distributed_e8s_equivalent += reward;
                }
                Err(e) => println!(
                    "{}Cannot find neuron {}, despite having voted with power {} \
                        in the considered reward period. The reward that should have been \
                        distributed to this neuron is simply skipped, so the total amount \
                        of distributed reward for this period will be lower than the maximum \
                        allowed. Underlying error: {:?}.",
                    LOG_PREFIX, neuron_id.id, used_voting_rights, e
                ),
            }
        }

        let now = self.env.now();
        for pid in considered_proposals.iter() {
            // Before considering a proposal for reward, it must be fully processed --
            // because we're about to clear the ballots, so no further processing will be
            // possible.
            self.process_proposal(pid.id);

            match self.mut_proposal_data(*pid) {
                None =>  println!(
                    "{}Cannot find proposal {}, despite it being considered for rewards distribution.",
                    LOG_PREFIX, pid.id
                ),
                Some(p) => {
                    if p.status() == ProposalStatus::Open {
                        println!("{}Proposal {} was considered for reward distribution despite \
                          being open. This code line is expected not to be reachable. We need to \
                          clear the ballots here to avoid a risk of the memory getting too large. \
                          In doubt, reject the proposal", LOG_PREFIX, pid.id);
                        p.decided_timestamp_seconds = now;
                        p.latest_tally = Some(Tally {
                            timestamp_seconds: now,
                            yes:0,
                            no:0,
                            total:0,
                       })
                    };
                    p.reward_event_round = day_after_genesis;
                    p.ballots.clear();
                }
            };
        }
        self.proto.latest_reward_event = Some(RewardEvent {
            day_after_genesis,
            actual_timestamp_seconds: now,
            settled_proposals: considered_proposals,
            distributed_e8s_equivalent: actually_distributed_e8s_equivalent,
        })
    }

    /// Recompute cached metrics once per day
    pub fn should_compute_cached_metrics(&self) -> bool {
        if let Some(metrics) = self.proto.metrics.as_ref() {
            let metrics_age_s = self.env.now() - metrics.timestamp_seconds;
            metrics_age_s > ONE_DAY_SECONDS
        } else {
            true
        }
    }

    /// Return the effective _voting period_ of a given topic.
    ///
    /// This function is "curried" to alleviate lifetime issues on the
    /// `self` parameter.
    pub fn voting_period_seconds(&self) -> impl Fn(Topic) -> u64 {
        let short = self.proto.short_voting_period_seconds;
        let normal = self.proto.wait_for_quiet_threshold_seconds;
        move |topic| {
            if topic == Topic::NeuronManagement || topic == Topic::ExchangeRate {
                short
            } else {
                normal
            }
        }
    }

    fn check_heap_can_grow(&self) -> Result<(), GovernanceError> {
        match self.env.heap_growth_potential() {
            HeapGrowthPotential::NoIssue => Ok(()),
            HeapGrowthPotential::LimitedAvailability => Err(GovernanceError::new_with_message(
                ErrorType::ResourceExhausted,
                "Heap size too large; governance canister is running is degraded mode.",
            )),
        }
    }

    /// Update the metadata for the given Node Provider
    pub fn update_node_provider(
        &mut self,
        node_provider_id: &PrincipalId,
        update: UpdateNodeProvider,
    ) -> Result<(), GovernanceError> {
        let node_provider = self
            .proto
            .node_providers
            .iter_mut()
            .find(|np| np.id.as_ref() == Some(node_provider_id))
            .ok_or_else(|| {
                GovernanceError::new_with_message(
                    ErrorType::NotFound,
                    format!("Node Provider {} is not known by the NNS", node_provider_id),
                )
            })?;

        if let Some(new_reward_account) = update.reward_account {
            node_provider.reward_account = Some(new_reward_account);
        } else {
            return Err(GovernanceError::new_with_message(
                ErrorType::PreconditionFailed,
                "reward_account not specified",
            ));
        }

        Ok(())
    }

    /// If the request is Committed, mint ICP and deposit it in the SNS
    /// governance canister's account. If the request is Aborted, refund
    /// Community Fund neurons that participated.
    ///
    /// Caller must be the swap canister, as recorded in the proposal.
    ///
    /// On success, sets the proposal's sns_token_swap_lifecycle accord to
    /// Committed or Aborted
    pub async fn settle_community_fund_participation(
        &mut self,
        caller: PrincipalId,
        request: &SettleCommunityFundParticipation,
    ) -> Result<(), GovernanceError> {
        validate_settle_community_fund_participation(request)?;

        // Look up proposal.
        let proposal_id = request
            .open_sns_token_swap_proposal_id
            .expect("The open_sns_token_swap_proposal_id field is not populated.");
        let proposal_data = match self.proto.proposals.get_mut(&proposal_id) {
            Some(pd) => pd,
            None => {
                return Err(GovernanceError::new_with_message(
                    ErrorType::NotFound,
                    format!(
                        "Proposal {} not found. request = {:#?}",
                        proposal_id, request
                    ),
                ))
            }
        };

        // Unpack proposal.
        let open_sns_token_swap = match proposal_data
            .proposal
            .as_ref()
            .and_then(|p| p.action.as_ref())
        {
            Some(Action::OpenSnsTokenSwap(open_sns_token_swap)) => open_sns_token_swap,
            _ => {
                return Err(GovernanceError::new_with_message(
                    ErrorType::NotFound,
                    format!(
                        "Proposal {} is not of type OpenSnsTokenSwap. request = {:#?}",
                        proposal_id, request,
                    ),
                ))
            }
        };

        // Check authorization.
        if Some(caller) != open_sns_token_swap.target_swap_canister_id {
            return Err(GovernanceError::new_with_message(
                ErrorType::NotAuthorized,
                format!(
                    "Caller was {}, but needs to be {:?}, the \
                     target_swap_canister_id in the original proposal.",
                    caller, open_sns_token_swap.target_swap_canister_id,
                ),
            ));
        }

        // It's possible that settle_community_fund_participation is called twice for a single Sale,
        // as such NNS Governance must treat this method as idempotent. If the proposal's
        // sns_token_swap_lifecycle is already set to Aborted or Committed (only done in a previous
        // call to settle_community_fund_participation), it is safe to do no work and return
        // success.
        if proposal_data
            .sns_token_swap_lifecycle
            .and_then(Lifecycle::from_i32)
            .unwrap_or(Lifecycle::Unspecified)
            .is_terminal()
        {
            println!(
                "{}INFO: settle_community_fund_participation was called for a Sale \
                    that has already been settled with ProposalId {:?}. Returning without \
                    doing additional work.",
                LOG_PREFIX, proposal_data.id
            );
            return Ok(());
        }

        // Get the type of request, i.e. Committed or Aborted.
        let request_type = match &request.result {
            None => {
                return Err(GovernanceError::new_with_message(
                    ErrorType::InvalidCommand,
                    format!(
                        "Request must be either Committed or Aborted, instead is None {:#?}",
                        request
                    ),
                ));
            }
            Some(request_type) => request_type,
        };

        // Record the proposal's current lifecycle. If an error occurs when settling the CF,
        // the previous Lifecycle should be set to allow for retries.
        let sns_token_swap_lifecycle_cache = proposal_data.sns_token_swap_lifecycle;

        // Set the lifecycle of the proposal to avoid interleaving callers
        proposal_data.set_sale_lifecycle_by_settle_cf_request_type(request_type);

        // Finally, execute.
        let settlement_result = match &request_type {
            settle_community_fund_participation::Result::Committed(committed) => {
                committed
                    .mint_to_sns_governance(proposal_data, &*self.ledger)
                    .await
            }

            settle_community_fund_participation::Result::Aborted(_aborted) => {
                let missing_neurons = refund_community_fund_maturity(
                    &mut self.proto.neurons,
                    &proposal_data.cf_participants,
                );
                println!(
                    "{}WARN: Neurons are missing from Governance when attempting to refund \
                    community fund participation in an SNS Sale. Missing Neurons: {:?}",
                    LOG_PREFIX, missing_neurons
                );
                Ok(())
            }
        };

        match settlement_result {
            Err(governance_error) => {
                // Reset the Proposal's lifecycle
                proposal_data.sns_token_swap_lifecycle = sns_token_swap_lifecycle_cache;
                Err(governance_error)
            }
            // Nothing to do, Lifecycle has already been updated
            Ok(()) => Ok(()),
        }
    }

    /// Return the given Node Provider, if it exists
    pub fn get_node_provider(
        &self,
        node_provider_id: &PrincipalId,
    ) -> Result<NodeProvider, GovernanceError> {
        // TODO(NNS1-1168): More efficient Node Provider lookup
        self.proto
            .node_providers
            .iter()
            .find(|np| np.id.as_ref() == Some(node_provider_id))
            .cloned()
            .ok_or_else(|| {
                GovernanceError::new_with_message(
                    ErrorType::NotFound,
                    format!("Node Provider {} is not known by the NNS", node_provider_id),
                )
            })
    }

    /// Return the monthly rewards that node providers should be awarded
    ///
    /// Fetches the map from node provider to monthly XDR rewards from the
    /// Registry, then fetches the average XDR to ICP conversion rate for
    /// the last 30 days, then applies this conversion rate to convert each
    /// node provider's XDR rewards to ICP.
    pub async fn get_monthly_node_provider_rewards(
        &self,
    ) -> Result<RewardNodeProviders, GovernanceError> {
        let mut rewards = RewardNodeProviders::default();

        // Maps node providers to their rewards in XDR
        let xdr_permyriad_rewards = get_node_providers_monthly_xdr_rewards().await?;

        // The average (last 30 days) conversion rate from 10,000ths of an XDR to 1 ICP
        let xdr_permyriad_per_icp = get_average_icp_xdr_conversion_rate()
            .await?
            .data
            .xdr_permyriad_per_icp;

        // Iterate over all node providers, calculate their rewards, and append them to
        // `rewards`
        for np in &self.proto.node_providers {
            if let Some(np_id) = &np.id {
                let np_id_str = np_id.to_string();
                let xdr_permyriad_reward =
                    *xdr_permyriad_rewards.rewards.get(&np_id_str).unwrap_or(&0);

                if let Some(reward_node_provider) =
                    get_node_provider_reward(np, xdr_permyriad_reward, xdr_permyriad_per_icp)
                {
                    rewards.rewards.push(reward_node_provider);
                }
            }
        }

        Ok(rewards)
    }

    /// Return the cached governance metrics.
    /// Governance metrics are updated once a day.
    pub fn get_metrics(&self) -> Result<GovernanceCachedMetrics, GovernanceError> {
        let metrics = &self.proto.metrics;
        match metrics {
            None => Err(GovernanceError::new_with_message(
                ErrorType::Unavailable,
                "Metrics not available",
            )),
            Some(m) => Ok(m.clone()),
        }
    }
}

// Returns whether the following requirements are met:
//   1. proposal must have a title.
//   2. title len (bytes, not characters) is between min and max.
pub fn validate_proposal_title(title: &Option<String>) -> Result<(), GovernanceError> {
    // Require that proposal has a title.
    let len = title
        .as_ref()
        .ok_or_else(|| {
            GovernanceError::new_with_message(ErrorType::InvalidProposal, "Proposal lacks a title")
        })?
        .len();

    // Require that title is not too short.
    if len < PROPOSAL_TITLE_BYTES_MIN {
        return Err(GovernanceError::new_with_message(
            ErrorType::InvalidProposal,
            format!(
                "Proposal title is too short (must be at least {} bytes)",
                PROPOSAL_TITLE_BYTES_MIN,
            ),
        ));
    }

    // Require that title is not too long.
    if len > PROPOSAL_TITLE_BYTES_MAX {
        return Err(GovernanceError::new_with_message(
            ErrorType::InvalidProposal,
            format!(
                "Proposal title is too long (can be at most {} bytes)",
                PROPOSAL_TITLE_BYTES_MAX,
            ),
        ));
    }

    Ok(())
}

/// Returns the amount of maturity held by all Community Fund neurons
/// (i.e. neurons with joined_community_fund_timestamp_seconds > 0).
#[must_use]
fn total_community_fund_maturity_e8s_equivalent(id_to_neuron: &HashMap<u64, Neuron>) -> u64 {
    id_to_neuron
        .values()
        .filter(|neuron| {
            neuron
                .joined_community_fund_timestamp_seconds
                .unwrap_or_default()
                > 0
        })
        .map(|neuron| neuron.maturity_e8s_equivalent)
        .sum()
}

/// Decrements maturity from Community Fund neurons (i.e. those with a nonzero
/// value in their joined_community_fund_timestamp_seconds field).
///
/// Each neuron whose maturity is taken has a corresponding entry in the return
/// value, which can be used as part of an OpenRequest sent to a SNS token
/// swap/sale canister.
fn draw_funds_from_the_community_fund(
    id_to_neuron: &mut HashMap<u64, Neuron>,
    original_total_community_fund_maturity_e8s_equivalent: u64,
    mut withdrawal_amount_e8s: u64,
    limits: &sns_swap_pb::Params,
) -> Vec<sns_swap_pb::CfParticipant> {
    if withdrawal_amount_e8s == 0 {
        return vec![];
    }

    let total_cf_maturity_e8s = total_community_fund_maturity_e8s_equivalent(id_to_neuron);
    if total_cf_maturity_e8s == 0 {
        return vec![];
    }
    if total_cf_maturity_e8s < original_total_community_fund_maturity_e8s_equivalent {
        // Scale down withdrawal amount, so that we do not use more maturity
        // than how it appeared when the proposal was first made.
        let scaled_down = (withdrawal_amount_e8s as u128) * (total_cf_maturity_e8s as u128)
            / (original_total_community_fund_maturity_e8s_equivalent as u128);
        assert!(
            scaled_down <= u64::MAX as u128,
            "scaled_down ({}) > u64::MAX",
            scaled_down
        );
        withdrawal_amount_e8s = scaled_down as u64;
    }

    // Cap the withdrawal amount.
    let original_withdrawal_amount_e8s = withdrawal_amount_e8s;
    let withdrawal_amount_e8s = withdrawal_amount_e8s
        .min(total_cf_maturity_e8s)
        // This is extra defensive programming, because OpenSnsTokenSwap
        // validation is supposed to ensure that withdrawal_amount_e8s <=
        // limits.max_icp_e8s.
        //
        // TODO: Maybe the withdrawal_amount_e8s should be (meaningfully) less
        // than max_icp_e8s, because otherwise, nobody else would be able to
        // participate.
        .min(limits.max_icp_e8s);

    // The amount that each CF neuron invests is proportional to its
    // maturity. Because we round down, there will almost certainly be some
    // short changing going on here. We could try to "fully top up", but it
    // doesn't seem worth the extra complexity, at least not for the time being.
    let mut principal_id_to_cf_neurons = HashMap::<PrincipalId, Vec<sns_swap_pb::CfNeuron>>::new();
    let mut captured_withdrawal_amount_e8s = 0;
    for neuron in id_to_neuron.values_mut() {
        let not_cf = neuron
            .joined_community_fund_timestamp_seconds
            .unwrap_or_default()
            == 0;
        if not_cf {
            continue;
        }

        // Make the current neuron's contribution proportional to its maturity.
        let neuron_contribution_e8s = (withdrawal_amount_e8s as u128)
            .saturating_mul(neuron.maturity_e8s_equivalent as u128)
            .saturating_div(total_cf_maturity_e8s as u128);
        assert!(
            neuron_contribution_e8s < (u64::MAX as u128),
            "{}",
            neuron_contribution_e8s
        );
        let mut neuron_contribution_e8s = neuron_contribution_e8s as u64;

        // Skip neurons that are too small. This can cause significant short
        // changing, much more so than rounding down.
        if neuron_contribution_e8s < limits.min_participant_icp_e8s {
            println!(
                "{}WARNING: Neuron {:?} is does not have enough maturity to participate \
                 in the current Community Fund investment.",
                LOG_PREFIX, &neuron.id,
            );
            continue;
        }

        // On the other extreme, don't let big CF neurons contribute too much
        // (by capping instead of skipping).
        if neuron_contribution_e8s > limits.max_participant_icp_e8s {
            let diff = neuron_contribution_e8s - limits.max_participant_icp_e8s;
            println!(
                "{}WARNING: Neuron {:?} has too much maturity to fully participate \
                 in the current SNS token swap. Therefore, its participation is \
                 being capped from {} to {} (a difference of {} or {}%).",
                LOG_PREFIX,
                &neuron.id,
                neuron_contribution_e8s,
                limits.max_participant_icp_e8s,
                diff,
                (diff as f64) / (neuron_contribution_e8s as f64),
            );
            neuron_contribution_e8s = limits.max_participant_icp_e8s;
        }

        // Create a record of this contribution.
        principal_id_to_cf_neurons
            .entry(neuron.controller.expect("Neuron has no controller."))
            .or_insert_with(Vec::new)
            .push(sns_swap_pb::CfNeuron {
                nns_neuron_id: neuron.id.as_ref().expect("Neuron lacks an id.").id,
                amount_icp_e8s: neuron_contribution_e8s,
            });

        // Deduct contribution from manturity.
        neuron.maturity_e8s_equivalent -= neuron_contribution_e8s;

        // Update running total.
        captured_withdrawal_amount_e8s += neuron_contribution_e8s;
    }

    // Convert principal_id_to_cf_neurons to the return type.
    let mut result = principal_id_to_cf_neurons
        .into_iter()
        .map(|(principal_id, cf_neurons)| sns_swap_pb::CfParticipant {
            hotkey_principal: principal_id.to_string(),
            cf_neurons,
        })
        .collect::<Vec<_>>();

    // Sort for predictable result. This just makes it easier to test.
    // Other than that, order doesn't matter.
    result.sort_by(|p1, p2| p1.hotkey_principal.cmp(&p2.hotkey_principal));
    // More predictability.
    for cf_participant in result.iter_mut() {
        cf_participant
            .cf_neurons
            .sort_by(|n1, n2| n1.nns_neuron_id.cmp(&n2.nns_neuron_id));
    }

    // Log the difference between the amount requested vs. actually captured.
    let diff_e8s =
        (original_withdrawal_amount_e8s as i128) - (captured_withdrawal_amount_e8s as i128);
    println!(
        "{}INFO: requested vs. captured Community Fund investment amount: {} - {} = {} ({} %)",
        LOG_PREFIX,
        original_withdrawal_amount_e8s,
        captured_withdrawal_amount_e8s,
        diff_e8s,
        100.0 * (diff_e8s as f64) / (original_withdrawal_amount_e8s as f64)
    );

    result
}

/// Reverts mutations performed by draw_funds_from_the_community_fund.
///
/// Returns elements where refunds failed (due to lack of a corresponding entry
/// in id_to_neuron). These can be used to create replacement/resurrected
/// neurons. Not done here, because that's a more disruptive change, which the
/// caller might not want to make.
#[must_use]
fn refund_community_fund_maturity(
    id_to_neuron: &mut HashMap<u64, Neuron>,
    cf_participants: &Vec<sns_swap_pb::CfParticipant>,
) -> Vec<sns_swap_pb::CfParticipant> {
    let mut result = vec![];

    for original_cf_participant in cf_participants {
        let mut failed_cf_participant = sns_swap_pb::CfParticipant {
            cf_neurons: vec![],
            ..original_cf_participant.clone()
        };

        for cf_neuron in &original_cf_participant.cf_neurons {
            match id_to_neuron.get_mut(&cf_neuron.nns_neuron_id) {
                Some(nns_neuron) => {
                    nns_neuron.maturity_e8s_equivalent += cf_neuron.amount_icp_e8s;
                    continue;
                }
                None => {
                    println!(
                        "{}WARNING: Refunding CF maturity is not proceeding cleanly, \
                         because a neuron has disappeared in the meantime. cf_neuron = {:#?}",
                        LOG_PREFIX, cf_neuron,
                    );
                    failed_cf_participant.cf_neurons.push(cf_neuron.clone());
                }
            }
        }

        if !failed_cf_participant.cf_neurons.is_empty() {
            result.push(failed_cf_participant);
        }
    }

    if !result.is_empty() {
        println!(
            "{}WARNING: Some Community Fund neurons seem to have gone \
             away while an SNS token swap they were participating was \
             going on, but that swap failed. failed_refunds = {:#?}",
            LOG_PREFIX, result,
        );
    }

    result
}

#[must_use]
fn sum_cf_participants_e8s(cf_participants: &[sns_swap_pb::CfParticipant]) -> u64 {
    let mut result = 0;
    for cf_participant in cf_participants {
        for cf_neuron in &cf_participant.cf_neurons {
            result += cf_neuron.amount_icp_e8s;
        }
    }
    result
}

fn validate_settle_community_fund_participation(
    request: &SettleCommunityFundParticipation,
) -> Result<(), GovernanceError> {
    let mut defects = vec![];

    if request.open_sns_token_swap_proposal_id.is_none() {
        defects.push("Lacks open_sns_token_swap_proposal_id.");
    }

    use settle_community_fund_participation::Result::{Aborted, Committed};
    match &request.result {
        None => {
            defects.push("Is neither Committed nor Aborted.");
        }
        Some(Aborted(_)) => (),
        Some(Committed(committed)) => {
            if committed.sns_governance_canister_id.is_none() {
                defects.push("Lacks sns_governance_canister_id.");
            }
        }
    }

    if defects.is_empty() {
        return Ok(());
    }

    Err(GovernanceError::new_with_message(
        ErrorType::InvalidCommand,
        format!(
            "SettleCommunityFundParticipation is invalid for the following reason(s):\n  - {}",
            defects.join("\n  - "),
        ),
    ))
}

fn validate_motion(motion: &Motion) -> Result<(), GovernanceError> {
    if motion.motion_text.len() > PROPOSAL_MOTION_TEXT_BYTES_MAX {
        return Err(GovernanceError::new_with_message(
            ErrorType::InvalidProposal,
            format!(
                "The maximum motion text size in a proposal action is {} bytes, this motion text is: {} bytes",
                PROPOSAL_MOTION_TEXT_BYTES_MAX,
                motion.motion_text.len()
            ),
        ));
    }

    Ok(())
}

/// Always fails, because this type of proposal is obsolete.
fn validate_set_sns_token_swap_open_time_window(
    action: &SetSnsTokenSwapOpenTimeWindow,
) -> Result<(), GovernanceError> {
    Err(GovernanceError::new_with_message(
        ErrorType::InvalidProposal,
        format!(
            "The SetSnsTokenSwapOpenTimeWindow proposal action is obsolete: {:?}",
            action,
        ),
    ))
}

async fn validate_open_sns_token_swap(
    open_sns_token_swap: &OpenSnsTokenSwap,
    env: &mut dyn Environment,
) -> Result<(), GovernanceError> {
    let mut defects = vec![];

    // Require target_swap_canister_id.
    let target_swap_canister_id = open_sns_token_swap.target_swap_canister_id;
    if target_swap_canister_id.is_none() {
        defects.push(
            "OpenSnsTokenSwap lacks a value in its target_swap_canister_id field.".to_string(),
        );
    }

    // Try to convert to CanisterId (from PrincipalId).
    let mut target_swap_canister_id = target_swap_canister_id.and_then(|id| {
        let result = CanisterId::try_from(id);

        if let Err(err) = &result {
            defects.push(format!(
                "OpenSnsTokenSwap.target_swap_canister_id is not a valid canister ID: {:?}",
                err,
            ));
        }

        // Convert to Option.
        result.ok()
    });

    // Is target_swap_canister_id known to sns_wasm ?
    if let Some(some_target_swap_canister_id) = target_swap_canister_id {
        let result = env
            .call_canister_method(
                SNS_WASM_CANISTER_ID,
                "list_deployed_snses",
                Encode!(&ListDeployedSnsesRequest {}).expect(""),
            )
            .await;

        let target_swap_canister_id_is_ok = match &result {
            Err(err) => {
                defects.push(format!(
                    "Failed to call the list_deployed_snses method on sns_wasm ({}): {:?}",
                    SNS_WASM_CANISTER_ID, err,
                ));
                false
            }

            Ok(reply_bytes) => match Decode!(reply_bytes, ListDeployedSnsesResponse) {
                Err(err) => {
                    defects.push(format!(
                        "Unable to decode response as ListDeployedSnsesResponse: {}. reply_bytes = {:#?}",
                        err, reply_bytes,
                    ));
                    false
                }

                Ok(response) => {
                    let is_swap = response.instances.iter().any(|sns| {
                        sns.swap_canister_id == Some(some_target_swap_canister_id.into())
                    });
                    if !is_swap {
                        defects.push(format!(
                            "target_swap_canister_id is not the ID of any swap canister \
                             known to sns_wasm: {}",
                            some_target_swap_canister_id
                        ));
                    }
                    is_swap
                }
            },
        };

        if !target_swap_canister_id_is_ok {
            target_swap_canister_id = None;
        }
    }

    // Inspect params.
    if let Some(target_swap_canister_id) = target_swap_canister_id {
        let result = validate_swap_params(
            env,
            target_swap_canister_id,
            open_sns_token_swap.params.as_ref(),
        )
        .await;
        if let Err(err) = result {
            defects.push(format!(
                "OpenSnsTokenSwap.params was invalid for the following \
                 reasons:\n{}",
                err,
            ));
        }
    }

    // community_fund_investment_e8s must be less than max_icp_e8s.
    if let Some(community_fund_investment_e8s) = open_sns_token_swap.community_fund_investment_e8s {
        if let Some(params) = &open_sns_token_swap.params {
            if community_fund_investment_e8s > params.max_icp_e8s {
                defects.push(format!(
                    "community_fund_investment_e8s ({}) > params.max_icp_e8s ({}).",
                    community_fund_investment_e8s, params.max_icp_e8s,
                ));
            }
        }
    }

    // Construct final result.
    if !defects.is_empty() {
        return Err(GovernanceError::new_with_message(
            ErrorType::InvalidProposal,
            defects.join("\n"),
        ));
    }
    Ok(())
}

async fn validate_swap_params(
    env: &mut dyn Environment,
    target_swap_canister_id: CanisterId,
    params: Option<&sns_swap_pb::Params>,
) -> Result<(), String> {
    let params = &params.ok_or("The `params` field in OpenSnsTokenSwap is not filled in.")?;

    // Get other data that we need to validate params from the swap canister.
    let result = env
        .call_canister_method(
            target_swap_canister_id,
            "get_state",
            Encode!(&sns_swap_pb::GetStateRequest {}).expect("Unable to encode GetStateRequest."),
        )
        .await;

    // Decode response.
    let response = result.map_err(|err| {
        format!(
            "Unable to validate OpenSnsTokenSwap.params because there was an error \
             while calling the get_state method of the swap canister {}: {:?}.",
            target_swap_canister_id, err,
        )
    })?;
    let response = Decode!(&response, sns_swap_pb::GetStateResponse).map_err(|err| {
        format!(
            "Unable to decode GetStateResponse from \
             swap canister (canister ID={}): {:#?}\nresponse:{:?}",
            target_swap_canister_id, err, response
        )
    })?;

    // Dig out Init from response.
    let init = match response {
        sns_swap_pb::GetStateResponse {
            swap: Some(sns_swap_pb::Swap {
                init: Some(init), ..
            }),
            ..
        } => init,
        _ => {
            return Err(format!(
                "Unable to get Init from GetStateResponse sent by swap \
             (canister ID={}): {:#?}",
                target_swap_canister_id, response
            ))
        }
    };

    // Now that we have all the ingredients, finally do the real work of
    // validating params.
    params.validate(&init)
}

/// A helper for the Registry's get_node_providers_monthly_xdr_rewards method
async fn get_node_providers_monthly_xdr_rewards(
) -> Result<NodeProvidersMonthlyXdrRewards, GovernanceError> {
    let registry_response: Result<
        Result<NodeProvidersMonthlyXdrRewards, String>,
        (Option<i32>, String),
    > = dfn_core::api::call_with_cleanup(
        REGISTRY_CANISTER_ID,
        "get_node_providers_monthly_xdr_rewards",
        candid_one,
        (),
    )
    .await;

    registry_response
        .map_err(|(code, msg)| {
            GovernanceError::new_with_message(
                ErrorType::External,
                format!(
                "Error calling 'get_node_providers_monthly_xdr_rewards': code: {:?}, message: {}",
                code, msg
            ),
            )
        })?
        .map_err(|msg| GovernanceError::new_with_message(ErrorType::External, msg))
}

/// A helper for the CMC's get_average_icp_xdr_conversion_rate method
async fn get_average_icp_xdr_conversion_rate(
) -> Result<IcpXdrConversionRateCertifiedResponse, GovernanceError> {
    let cmc_response: Result<IcpXdrConversionRateCertifiedResponse, (Option<i32>, String)> =
        dfn_core::api::call_with_cleanup(
            CYCLES_MINTING_CANISTER_ID,
            "get_average_icp_xdr_conversion_rate",
            candid_one,
            (),
        )
        .await;

    cmc_response.map_err(|(code, msg)| {
        GovernanceError::new_with_message(
            ErrorType::External,
            format!(
                "Error calling 'get_average_icp_xdr_conversion_rate': code: {:?}, message: {}",
                code, msg
            ),
        )
    })
}

/// Given the XDR amount that the given node provider should be rewarded, and a
/// conversion rate from XDR to ICP, returns the ICP amount and wallet address
/// that should be awarded on behalf of the given node provider.
///
/// The simple way to calculate this might be:
/// xdr_permyriad_reward / xdr_permyriad_per_icp
/// or more explicitly:
/// $reward_amount XDR / ( $rate XDR / 1 ICP)
/// ==
/// $reward_amount XDR * (1 ICP / $rate XDR)
/// ==
/// ($reward_amount / $rate) ICP
///
/// However this discards e8s. In order to account for e8s, we convert ICP to
/// e8s using `TOKEN_SUBDIVIDABLE_BY`:
/// $reward_amount XDR * (TOKEN_SUBDIVIDABLE_BY e8s / 1 ICP) * (1 ICP / $rate
/// XDR) ==
/// $reward_amount XDR * (TOKEN_SUBDIVIDABLE_BY e8s / $rate XDR)
/// ==
/// (($reward_amount * TOKEN_SUBDIVIDABLE_BY) / $rate) e8s
fn get_node_provider_reward(
    np: &NodeProvider,
    xdr_permyriad_reward: u64,
    xdr_permyriad_per_icp: u64,
) -> Option<RewardNodeProvider> {
    if let Some(np_id) = np.id.as_ref() {
        let amount_e8s = ((xdr_permyriad_reward as u128 * TOKEN_SUBDIVIDABLE_BY as u128)
            / xdr_permyriad_per_icp as u128) as u64;

        let to_account = Some(if let Some(account) = &np.reward_account {
            account.clone()
        } else {
            AccountIdentifier::from(*np_id).into()
        });

        Some(RewardNodeProvider {
            node_provider: Some(np.clone()),
            amount_e8s,
            reward_mode: Some(RewardMode::RewardToAccount(RewardToAccount { to_account })),
        })
    } else {
        None
    }
}

impl settle_community_fund_participation::Committed {
    async fn mint_to_sns_governance(
        &self,
        proposal_data: &ProposalData,
        ledger: &'_ dyn IcpLedger,
    ) -> Result<(), GovernanceError> {
        let amount_e8s = sum_cf_participants_e8s(&proposal_data.cf_participants);

        // Send request to ICP ledger.
        let owner = self
            .sns_governance_canister_id
            .ok_or_else(|| GovernanceError::new_with_message(
                ErrorType::NotFound,
                "Expected sns_governance_canister_id to be set in SettleCommunityFundParticipation::Committed Request"
            ))?;
        let destination = AccountIdentifier::new(owner, /* subaccount = */ None);
        let ledger_result = ledger
            .transfer_funds(
                amount_e8s,
                /* fee_e8s = */ 0, // Because there is no fee for minting.
                /* from_subaccount = */ None,
                destination,
                /* memo = */ 0,
            )
            .await;

        // Convert result.
        match ledger_result {
            Ok(_) => Ok(()),
            Err(err) => Err(GovernanceError::new_with_message(
                ErrorType::External,
                format!(
                    "Minting ICP from the Community Fund failed: \
                     err = {:#?}. proposal_data = {:#?}",
                    err, proposal_data,
                ),
            )),
        }
    }
}

async fn fetch_swap_background_information(
    env: &mut dyn Environment,
    target_swap_canister_id: CanisterId,
) -> Result<SwapBackgroundInformation, GovernanceError> {
    // Call the swap canister's `get_state` method.
    let swap_get_state_result = env
        .call_canister_method(
            target_swap_canister_id,
            "get_state",
            Encode!(&sns_swap_pb::GetStateRequest {}).expect("Unable to encode a GetStateRequest."),
        )
        .await;
    let swap_get_state_response = match swap_get_state_result {
        Err(err) => {
            return Err(GovernanceError::new_with_message(
                ErrorType::External,
                format!(
                    "get_state call to swap {} to failed: {:?}",
                    target_swap_canister_id, err,
                ),
            ));
        }
        Ok(reply_bytes) => Decode!(&reply_bytes, sns_swap_pb::GetStateResponse)
            .expect("Unable to decode GetStateResponse."),
    };
    let swap_init = swap_get_state_response
        .swap
        .expect("`swap` field is not set in GetStateResponse.")
        .init
        .expect("`init` field is not set in GetStateResponse.swap.");

    // Call the SNS root canister's `get_sns_canisters_summary` method.
    // TODO IC-1448 - This panic will eventually go away when SNS Governance
    // no longer depends on the Sale canister to provide this data.
    let sns_root_canister_id = swap_init.sns_root_or_panic();
    let get_sns_canisters_summary_result = env
        .call_canister_method(
            sns_root_canister_id,
            "get_sns_canisters_summary",
            Encode!(&GetSnsCanistersSummaryRequest {
                update_canister_list: None
            })
            .expect("Unable to encode a GetSnsCanistersSummaryRequest."),
        )
        .await;
    let get_sns_canisters_summary_response = match get_sns_canisters_summary_result {
        Err(err) => {
            return Err(GovernanceError::new_with_message(
                ErrorType::External,
                format!(
                    "get_sns_canisters_summary call to root {} to failed: {:?}",
                    sns_root_canister_id, err,
                ),
            ));
        }

        Ok(reply_bytes) => Decode!(&reply_bytes, ic_sns_root::GetSnsCanistersSummaryResponse)
            .unwrap_or_else(|err| {
                panic!(
                    "Unable to decode {} bytes into a GetSnsCanistersSummaryResponse: {:?}",
                    reply_bytes.len(),
                    err,
                )
            }),
    };

    // Double check that swap and root agree on IDs of sister canisters. This
    // should never be a problem; we are just being extra defensive here.
    let ok = is_information_about_swap_from_different_sources_consistent(
        &get_sns_canisters_summary_response,
        &swap_init,
        PrincipalId::from(target_swap_canister_id),
    );
    if !ok {
        return Err(GovernanceError::new_with_message(
            ErrorType::External,
            format!(
                "Inconsistent value(s) from root and swap canisters:\n\
                 get_sns_canisters_summary_response = {:#?}\n\
                 vs.\n\
                 swap_init = {:#?}\n\
                 vs.\n\
                 target_swap_canister_id = {}",
                get_sns_canisters_summary_response, swap_init, target_swap_canister_id,
            ),
        ));
    }

    // Repackage everything we just fetched into a deduplicated form.
    let fallback_controller_principal_ids = swap_init
        .fallback_controller_principal_ids
        .iter()
        .map(|string| {
            PrincipalId::from_str(string).unwrap_or_else(|err| {
                panic!("Could not parse {:?} as a PrincipalId: {:?}", string, err)
            })
        })
        .collect::<Vec<_>>();
    Ok(SwapBackgroundInformation::new(
        &fallback_controller_principal_ids,
        &get_sns_canisters_summary_response,
    ))
}

fn is_information_about_swap_from_different_sources_consistent(
    get_sns_canisters_summary_response: &GetSnsCanistersSummaryResponse,
    swap_init: &sns_swap_pb::Init,
    target_swap_canister_id: PrincipalId,
) -> bool {
    match get_sns_canisters_summary_response {
        GetSnsCanistersSummaryResponse {
            root:
                Some(ic_sns_root::CanisterSummary {
                    canister_id: Some(root_sns_root_canister_id),
                    ..
                }),
            governance:
                Some(ic_sns_root::CanisterSummary {
                    canister_id: Some(root_sns_governance_canister_id),
                    ..
                }),
            ledger:
                Some(ic_sns_root::CanisterSummary {
                    canister_id: Some(root_sns_ledger_canister_id),
                    ..
                }),
            swap:
                Some(ic_sns_root::CanisterSummary {
                    canister_id: Some(root_sns_swap_canister_id),
                    ..
                }),

            archives: _,
            index:
                Some(ic_sns_root::CanisterSummary {
                    canister_id: Some(_),
                    ..
                }),

            dapps: _,
        } => {
            // Extract fields from swap_init.
            let sns_swap_pb::Init {
                sns_governance_canister_id: swap_sns_governance_canister_id,
                sns_ledger_canister_id: swap_sns_ledger_canister_id,
                sns_root_canister_id: swap_sns_root_canister_id,

                fallback_controller_principal_ids: _,

                nns_governance_canister_id: _,
                icp_ledger_canister_id: _,
                transaction_fee_e8s: _,
                neuron_minimum_stake_e8s: _,
            } = swap_init;

            (
                swap_sns_root_canister_id,
                swap_sns_governance_canister_id,
                swap_sns_ledger_canister_id,
                target_swap_canister_id,
            ) == (
                &root_sns_root_canister_id.to_string(),
                &root_sns_governance_canister_id.to_string(),
                &root_sns_ledger_canister_id.to_string(),
                *root_sns_swap_canister_id,
            )
        }
        _ => false,
    }
}

impl SwapBackgroundInformation {
    fn new(
        fallback_controller_principal_ids: &[PrincipalId],
        get_sns_canisters_summary_response: &GetSnsCanistersSummaryResponse,
    ) -> Self {
        // Extract field values from get_sns_canisters_summary_response.
        let GetSnsCanistersSummaryResponse {
            root: root_canister_summary,
            governance: governance_canister_summary,
            ledger: ledger_canister_summary,
            swap: swap_canister_summary,
            dapps: dapp_canister_summaries,
            archives: ledger_archive_canister_summaries,
            index: ledger_index_canister_summary,
        } = get_sns_canisters_summary_response;

        // Convert field values to analogous PB types.
        let root_canister_summary = root_canister_summary.as_ref().map(|s| s.into());
        let governance_canister_summary = governance_canister_summary.as_ref().map(|s| s.into());
        let ledger_canister_summary = ledger_canister_summary.as_ref().map(|s| s.into());
        let swap_canister_summary = swap_canister_summary.as_ref().map(|s| s.into());
        let ledger_index_canister_summary =
            ledger_index_canister_summary.as_ref().map(|s| s.into());

        let dapp_canister_summaries = dapp_canister_summaries
            .iter()
            .map(|s| s.into())
            .collect::<Vec<_>>();
        let ledger_archive_canister_summaries = ledger_archive_canister_summaries
            .iter()
            .map(|s| s.into())
            .collect::<Vec<_>>();

        let fallback_controller_principal_ids = fallback_controller_principal_ids.into();

        Self {
            // Primary SNS Canisters
            root_canister_summary,
            governance_canister_summary,
            ledger_canister_summary,
            swap_canister_summary,

            // Secondary SNS Canisters
            ledger_archive_canister_summaries,
            ledger_index_canister_summary,

            // Application
            dapp_canister_summaries,
            fallback_controller_principal_ids,
        }
    }
}

impl From<&ic_sns_root::CanisterSummary> for swap_background_information::CanisterSummary {
    fn from(src: &ic_sns_root::CanisterSummary) -> Self {
        let ic_sns_root::CanisterSummary {
            canister_id,
            status,
        } = src;

        let canister_id = *canister_id;
        let status = status.as_ref().map(|status| status.into());

        Self {
            canister_id,
            status,
        }
    }
}

impl From<&ic_sns_root::CanisterStatusResultV2>
    for swap_background_information::CanisterStatusResultV2
{
    fn from(src: &ic_sns_root::CanisterStatusResultV2) -> Self {
        // Extract from src.
        let status = src.status();
        let module_hash = src.module_hash();
        let controllers = src.controllers();
        let memory_size = src.memory_size();
        let cycles = src.cycles();
        let freezing_threshold = src.freezing_threshold();
        let idle_cycles_burned_per_day = src.idle_cycles_burned_per_day();

        // Convert data extracted from src.
        let status = swap_background_information::CanisterStatusType::from(status);
        let module_hash = module_hash.unwrap_or_else(|| vec![]);
        let cycles = u64::try_from(cycles).unwrap_or_else(|err| {
            println!(
                "{}WARNING: Unable to convert cycles to u64: {:?}",
                LOG_PREFIX, err,
            );
            u64::MAX
        });
        let idle_cycles_burned_per_day =
            u64::try_from(idle_cycles_burned_per_day).unwrap_or_else(|err| {
                println!(
                    "{}WARNING: Unable to convert idle_cycles_burned_per_day to u64: {:?}",
                    LOG_PREFIX, err,
                );
                u64::MAX
            });

        // Repackage into PB type.
        Self {
            status: Some(status as i32),
            module_hash,
            controllers,
            memory_size: Some(memory_size.get()),
            cycles: Some(cycles),
            freezing_threshold: Some(freezing_threshold),
            idle_cycles_burned_per_day: Some(idle_cycles_burned_per_day),
        }
    }
}

impl From<ic_sns_root::CanisterStatusType> for swap_background_information::CanisterStatusType {
    fn from(src: ic_sns_root::CanisterStatusType) -> Self {
        use ic_sns_root::CanisterStatusType as Src;

        match src {
            Src::Running => Self::Running,
            Src::Stopping => Self::Stopping,
            Src::Stopped => Self::Stopped,
        }
    }
}

/// Affects the perception of time by users of CanisterEnv (i.e. Governance).
///
/// Specifically, the time that Governance sees is the real time + delta.
#[derive(PartialEq, Eq, Clone, Copy, Debug, candid::CandidType, serde::Deserialize)]
pub struct TimeWarp {
    pub delta_s: i64,
}

impl TimeWarp {
    pub fn apply(&self, timestamp_s: u64) -> u64 {
        if self.delta_s >= 0 {
            timestamp_s + (self.delta_s as u64)
        } else {
            timestamp_s - ((-self.delta_s) as u64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_nervous_system_common::{assert_is_err, assert_is_ok, E8};
    use ic_sns_swap::pb::v1::{params::NeuronBasketConstructionParameters, Swap};
    use ic_sns_wasm::pb::v1::DeployedSns;
    use lazy_static::lazy_static;
    use maplit::hashmap;
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    #[test]
    fn test_time_warp() {
        let w = TimeWarp { delta_s: 0_i64 };
        assert_eq!(w.apply(100_u64), 100);

        let w = TimeWarp { delta_s: 42_i64 };
        assert_eq!(w.apply(100_u64), 142);

        let w = TimeWarp { delta_s: -42_i64 };
        assert_eq!(w.apply(100_u64), 58);
    }

    const PARAMS: sns_swap_pb::Params = sns_swap_pb::Params {
        max_icp_e8s: 1000 * E8,
        min_icp_e8s: 10 * E8,
        min_participant_icp_e8s: 5 * E8,
        max_participant_icp_e8s: 1000 * E8,
        min_participants: 2,
        sns_token_e8s: 1000 * E8,
        swap_due_timestamp_seconds: 2524629600, // midnight, Jan 1, 2050
        neuron_basket_construction_parameters: Some(
            sns_swap_pb::params::NeuronBasketConstructionParameters {
                count: 3,
                dissolve_delay_interval_seconds: 7890000, // 3 months
            },
        ),
        sale_delay_seconds: None,
    };

    type CanisterMethodCallResult = Result<Vec<u8>, (Option<i32>, String)>;

    lazy_static! {
        static ref PRINCIPAL_ID_1: PrincipalId = PrincipalId::new_user_test_id(1);
        static ref PRINCIPAL_ID_2: PrincipalId = PrincipalId::new_user_test_id(2);
        static ref TARGET_SWAP_CANISTER_ID: CanisterId = CanisterId::from_u64(435106);
        static ref OPEN_SNS_TOKEN_SWAP: OpenSnsTokenSwap = OpenSnsTokenSwap {
            target_swap_canister_id: Some((*TARGET_SWAP_CANISTER_ID).into()),
            params: Some(PARAMS.clone()),
            community_fund_investment_e8s: Some(500),
        };
        static ref SWAP_INIT: sns_swap_pb::Init = sns_swap_pb::Init {
            transaction_fee_e8s: Some(12_345),
            neuron_minimum_stake_e8s: Some(123_456_789),
            ..Default::default() // Not realistic, but good enough for tests.
        };

        static ref EXPECTED_LIST_DEPLOYED_SNSES: (ExpectedCallCanisterMethodCallArguments<'static>, CanisterMethodCallResult) =
            (
                ExpectedCallCanisterMethodCallArguments {
                    target: SNS_WASM_CANISTER_ID,
                    method_name: "list_deployed_snses",
                    request: Encode!(&ListDeployedSnsesRequest {}).unwrap(),
                },
                Ok(Encode!(&ListDeployedSnsesResponse {
                    instances: vec![DeployedSns {
                        swap_canister_id: Some((*TARGET_SWAP_CANISTER_ID).into()),
                        ..Default::default()
                    },]
                })
                   .unwrap()),
            );
    }

    #[derive(Debug, PartialEq, Eq, Clone)]
    struct ExpectedCallCanisterMethodCallArguments<'a> {
        target: CanisterId,
        method_name: &'a str,
        request: Vec<u8>,
    }

    #[allow(clippy::type_complexity)]
    struct MockEnvironment<'a> {
        expected_call_canister_method_calls: Arc<
            Mutex<
                VecDeque<(
                    ExpectedCallCanisterMethodCallArguments<'a>,
                    Result<Vec<u8>, (Option<i32>, String)>,
                )>,
            >,
        >,
    }

    impl Default for MockEnvironment<'_> {
        fn default() -> Self {
            Self {
                expected_call_canister_method_calls: Arc::new(Mutex::new(VecDeque::from([
                    EXPECTED_LIST_DEPLOYED_SNSES.clone(),
                    (
                        ExpectedCallCanisterMethodCallArguments {
                            target: *TARGET_SWAP_CANISTER_ID,
                            method_name: "get_state",
                            request: Encode!(&sns_swap_pb::GetStateRequest {}).unwrap(),
                        },
                        Ok(Encode!(&sns_swap_pb::GetStateResponse {
                            swap: Some(Swap {
                                init: Some(SWAP_INIT.clone()),
                                ..Default::default() // Not realistic, but good enough for test.
                            }),
                            derived: None, // Not realistic, but good enough for test.
                        })
                        .unwrap()),
                    ),
                ]))),
            }
        }
    }

    #[async_trait]
    impl Environment for MockEnvironment<'_> {
        fn now(&self) -> u64 {
            unimplemented!();
        }

        fn random_u64(&mut self) -> u64 {
            unimplemented!();
        }

        fn random_byte_array(&mut self) -> [u8; 32] {
            unimplemented!();
        }

        fn execute_nns_function(
            &self,
            _proposal_id: u64,
            _update: &ExecuteNnsFunction,
        ) -> Result<(), GovernanceError> {
            unimplemented!();
        }

        fn heap_growth_potential(&self) -> HeapGrowthPotential {
            unimplemented!();
        }

        async fn call_canister_method(
            &mut self,
            target: CanisterId,
            method_name: &str,
            request: Vec<u8>,
        ) -> Result<Vec<u8>, (Option<i32>, String)> {
            let (expected_arguments, result) = self
                .expected_call_canister_method_calls
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    panic!(
                        "Unexpected canister method call:\n\
                     method_name = {}\n\
                     target = {}\n\
                     request.len() = {}",
                        method_name,
                        target,
                        request.len(),
                    )
                });

            let decode_request_bytes = |bytes| {
                match method_name {
                    "get_state" => {
                        match Decode!(bytes, sns_swap_pb::GetStateRequest) {
                            Ok(ok) => format!("{:#?}", ok),
                            Err(err) => format!(
                                "Unable to decode request bytes as GetStateRequest because of {:?}: {}",
                                err, default_request_bytes_format(bytes),
                            ),
                        }
                    }

                    "list_deployed_snses" => {
                        match Decode!(bytes, ListDeployedSnsesRequest) {
                            Ok(ok) => format!("{:#?}", ok),
                            Err(err) => format!(
                                "Unable to decode request bytes as ListDeployedSnsesRequest because of {:?}: {}",
                                err, default_request_bytes_format(bytes),
                            ),
                        }
                    }

                    _ => default_request_bytes_format(bytes)
                }
            };
            fn default_request_bytes_format(bytes: &[u8]) -> String {
                let truncated = if bytes.len() > 16 {
                    let head = &bytes[..8];
                    let tail = &bytes[(bytes.len() - 8)..];
                    format!("head = {:?}, tail = {:?}", head, tail)
                } else {
                    format!("content = {:?}", bytes)
                };

                format!("<len = {}, {}>", bytes.len(), truncated)
            }

            // Compare incoming arguments vs. expected.
            assert_eq!(
                method_name, expected_arguments.method_name,
                "{:#?}",
                expected_arguments
            );
            assert_eq!(
                target, expected_arguments.target,
                "{:#?}",
                expected_arguments
            );
            assert!(
                // Because these are Vec<u8>, assert_eq would generate feedback
                // that's very hard to decypher, so we skip that by using
                // assert! plus the == operator instead.
                request == expected_arguments.request,
                "{}\nvs.\n{}",
                decode_request_bytes(&request),
                decode_request_bytes(&expected_arguments.request),
            );

            result
        }
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_ok() {
        let result =
            validate_open_sns_token_swap(&OPEN_SNS_TOKEN_SWAP, &mut MockEnvironment::default())
                .await;
        assert!(result.is_ok(), "{:#?}", result);
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_missing_target_swap_canister_id() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OpenSnsTokenSwap {
                    target_swap_canister_id: None,
                    ..OPEN_SNS_TOKEN_SWAP.clone()
                },
                &mut MockEnvironment::default(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_params_no_params() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OpenSnsTokenSwap {
                    params: None,
                    ..OPEN_SNS_TOKEN_SWAP.clone()
                },
                &mut MockEnvironment::default(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_sns_wasm_list_deployed_snses_fail() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OPEN_SNS_TOKEN_SWAP,
                &mut MockEnvironment {
                    expected_call_canister_method_calls: Arc::new(Mutex::new(VecDeque::from([(
                        ExpectedCallCanisterMethodCallArguments {
                            target: SNS_WASM_CANISTER_ID,
                            method_name: "list_deployed_snses",
                            request: Encode!(&ListDeployedSnsesRequest {}).unwrap(),
                        },
                        Err((None, "derp".to_string())),
                    ),]),)),
                },
            )
            .await
        );
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_unknown_swap() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OPEN_SNS_TOKEN_SWAP,
                &mut MockEnvironment {
                    expected_call_canister_method_calls: Arc::new(Mutex::new(VecDeque::from([(
                        ExpectedCallCanisterMethodCallArguments {
                            target: SNS_WASM_CANISTER_ID,
                            method_name: "list_deployed_snses",
                            request: Encode!(&ListDeployedSnsesRequest {}).unwrap(),
                        },
                        Ok(Encode!(&ListDeployedSnsesResponse { instances: vec![] }).unwrap()),
                    )]))),
                },
            )
            .await
        );
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_swap_get_state_fail() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OPEN_SNS_TOKEN_SWAP,
                &mut MockEnvironment {
                    expected_call_canister_method_calls: Arc::new(Mutex::new(VecDeque::from([
                        EXPECTED_LIST_DEPLOYED_SNSES.clone(),
                        (
                            ExpectedCallCanisterMethodCallArguments {
                                target: *TARGET_SWAP_CANISTER_ID,
                                method_name: "get_state",
                                request: Encode!(&sns_swap_pb::GetStateRequest {}).unwrap(),
                            },
                            Err((None, "derp".to_string())),
                        )
                    ]),)),
                },
            )
            .await
        );
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_params_max_icp_e8s_too_small() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OpenSnsTokenSwap {
                    params: Some(sns_swap_pb::Params {
                        max_icp_e8s: 1, // Too small.
                        ..PARAMS.clone()
                    }),
                    ..OPEN_SNS_TOKEN_SWAP.clone()
                },
                &mut MockEnvironment::default(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_params_basket_count_too_small() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OpenSnsTokenSwap {
                    params: Some(sns_swap_pb::Params {
                        neuron_basket_construction_parameters: Some(
                            NeuronBasketConstructionParameters {
                                count: 0,                                 // Too small
                                dissolve_delay_interval_seconds: 7890000, // 3 months
                            },
                        ),
                        ..PARAMS.clone()
                    }),
                    ..OPEN_SNS_TOKEN_SWAP.clone()
                },
                &mut MockEnvironment::default(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_params_zero_dissolve_delay() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OpenSnsTokenSwap {
                    params: Some(sns_swap_pb::Params {
                        neuron_basket_construction_parameters: Some(
                            NeuronBasketConstructionParameters {
                                count: 12,
                                dissolve_delay_interval_seconds: 0, // Too small
                            },
                        ),
                        ..PARAMS.clone()
                    }),
                    ..OPEN_SNS_TOKEN_SWAP.clone()
                },
                &mut MockEnvironment::default(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_params_practically_forever_dissolve_delay() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OpenSnsTokenSwap {
                    params: Some(sns_swap_pb::Params {
                        neuron_basket_construction_parameters: Some(
                            NeuronBasketConstructionParameters {
                                count: 2,
                                dissolve_delay_interval_seconds: u64::MAX, // Will result in overflow
                            },
                        ),
                        ..PARAMS.clone()
                    }),
                    ..OPEN_SNS_TOKEN_SWAP.clone()
                },
                &mut MockEnvironment::default(),
            )
            .await
        );
    }

    #[tokio::test]
    async fn validate_open_sns_token_swap_community_fund_investment_e8s() {
        assert_is_err!(
            validate_open_sns_token_swap(
                &OpenSnsTokenSwap {
                    community_fund_investment_e8s: Some(1001 * E8), // Exceeds max_icp_e8s.
                    ..OPEN_SNS_TOKEN_SWAP.clone()
                },
                &mut MockEnvironment::default(),
            )
            .await
        );
    }

    lazy_static! {
        static ref ID_TO_NEURON: HashMap<u64, Neuron> = craft_id_to_neuron(&[
            // (maturity, controller, joined cf at)

            // CF neurons.
            (100 * E8, *PRINCIPAL_ID_1, Some(1)),
            (200 * E8, *PRINCIPAL_ID_2, Some(1)),
            (300 * E8, *PRINCIPAL_ID_1, Some(1)),

            // non-CF neurons.
            (400 * E8, *PRINCIPAL_ID_1, None),
            (500 * E8, *PRINCIPAL_ID_2, None),
        ]);

        static ref ORIGINAL_TOTAL_COMMUNITY_FUND_MATURITY_E8S_EQUIVALENT: u64 = {
            let result = total_community_fund_maturity_e8s_equivalent(&ID_TO_NEURON);
            assert_eq!(result, 600 * E8);
            result
        };
    }

    fn craft_id_to_neuron(
        values: &[(
            /* maturity: */ u64,
            /* controller: */ PrincipalId,
            /* joined cf at: */ Option<u64>,
        )],
    ) -> HashMap<u64, Neuron> {
        values
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let i = i as u64;
                let (maturity_e8s_equivalent, controller, joined_community_fund_timestamp_seconds) =
                    *arg;

                let id = i + 1;
                let neuron = Neuron {
                    id: Some(NeuronId { id }),
                    controller: Some(controller),
                    maturity_e8s_equivalent,
                    joined_community_fund_timestamp_seconds,
                    ..Default::default()
                };

                (id, neuron)
            })
            .collect()
    }

    fn assert_clean_refund(
        id_to_neuron: &mut HashMap<u64, Neuron>,
        cf_participants: &Vec<sns_swap_pb::CfParticipant>,
        expected_id_to_neuron: &HashMap<u64, Neuron>,
    ) {
        let mut original_id_to_neuron = id_to_neuron.clone();
        let failed_refunds = refund_community_fund_maturity(id_to_neuron, cf_participants);
        assert!(failed_refunds.is_empty(), "{:#?}", failed_refunds);

        // Assert that neurons have been restored to the way they were originally.
        assert_eq!(id_to_neuron, expected_id_to_neuron);

        // Assert that inserting extraneous elements into cf_participants does
        // not change the result, but it does result in failed refunds.
        let mut extra_cf_participants = cf_participants.clone();
        let mut expected_failed_refunds = vec![];
        if !extra_cf_participants.is_empty() {
            let cf_neuron = sns_swap_pb::CfNeuron {
                nns_neuron_id: 688477,
                amount_icp_e8s: 592,
            };
            extra_cf_participants
                .get_mut(0)
                .unwrap()
                .cf_neurons
                .push(cf_neuron.clone());
            expected_failed_refunds.push(sns_swap_pb::CfParticipant {
                hotkey_principal: extra_cf_participants
                    .get(0)
                    .unwrap()
                    .hotkey_principal
                    .clone(),
                cf_neurons: vec![cf_neuron],
            });
        }

        let cf_participant = sns_swap_pb::CfParticipant {
            hotkey_principal: PrincipalId::new_user_test_id(301590).to_string(),
            cf_neurons: vec![
                sns_swap_pb::CfNeuron {
                    nns_neuron_id: 875889,
                    amount_icp_e8s: 591,
                },
                sns_swap_pb::CfNeuron {
                    nns_neuron_id: 734429,
                    amount_icp_e8s: 917,
                },
            ],
        };
        extra_cf_participants.push(cf_participant.clone());
        expected_failed_refunds.push(cf_participant);

        assert_eq!(
            refund_community_fund_maturity(&mut original_id_to_neuron, &extra_cf_participants),
            expected_failed_refunds,
        );
        assert_eq!(original_id_to_neuron, *expected_id_to_neuron);
    }

    #[test]
    fn draw_funds_from_the_community_fund_all_cf_neurons_have_zero_maturity() {
        let mut id_to_neuron = craft_id_to_neuron(&[
            // (maturity, controller, joined cf at)

            // CF neurons.
            (0, *PRINCIPAL_ID_1, Some(1)),
            (0, *PRINCIPAL_ID_2, Some(1)),
            (0, *PRINCIPAL_ID_1, Some(1)),
            // non-CF neurons.
            (400, *PRINCIPAL_ID_1, None),
            (500, *PRINCIPAL_ID_2, None),
        ]);
        let original_id_to_neuron = id_to_neuron.clone();

        let observed_cf_neurons = draw_funds_from_the_community_fund(
            &mut id_to_neuron,
            *ORIGINAL_TOTAL_COMMUNITY_FUND_MATURITY_E8S_EQUIVALENT,
            /* withdrawal_amount_e8s = */ 60,
            &PARAMS,
        );

        // Inspect results.
        assert_eq!(observed_cf_neurons, vec![]);
        assert_eq!(id_to_neuron, original_id_to_neuron);
        assert_clean_refund(
            &mut id_to_neuron,
            &observed_cf_neurons,
            &original_id_to_neuron,
        );
    }

    #[test]
    fn draw_funds_from_the_community_fund_zero_withdrawal_amount() {
        let mut id_to_neuron = craft_id_to_neuron(&[
            // (maturity, controller, joined cf at)

            // CF neurons.
            (0, *PRINCIPAL_ID_1, Some(1)),
            (10, *PRINCIPAL_ID_2, Some(1)),
            (50, *PRINCIPAL_ID_1, Some(1)),
            // non-CF neurons.
            (400, *PRINCIPAL_ID_1, None),
            (500, *PRINCIPAL_ID_2, None),
        ]);
        let original_id_to_neuron = id_to_neuron.clone();

        let observed_cf_neurons = draw_funds_from_the_community_fund(
            &mut id_to_neuron,
            *ORIGINAL_TOTAL_COMMUNITY_FUND_MATURITY_E8S_EQUIVALENT,
            /* withdrawal_amount_e8s = */ 0,
            &PARAMS,
        );

        // Inspect results.
        assert_eq!(observed_cf_neurons, vec![]);
        assert_eq!(id_to_neuron, original_id_to_neuron);
        assert_clean_refund(
            &mut id_to_neuron,
            &observed_cf_neurons,
            &original_id_to_neuron,
        );
    }

    #[test]
    fn draw_funds_from_the_community_fund_typical() {
        let mut id_to_neuron = ID_TO_NEURON.clone();

        let observed_cf_neurons = draw_funds_from_the_community_fund(
            &mut id_to_neuron,
            *ORIGINAL_TOTAL_COMMUNITY_FUND_MATURITY_E8S_EQUIVALENT,
            /* withdrawal_amount_e8s = */ 60 * E8,
            &PARAMS,
        );

        // Inspect results.

        let mut expected_cf_neurons = vec![
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_1.to_string(),
                cf_neurons: vec![
                    sns_swap_pb::CfNeuron {
                        nns_neuron_id: 1,
                        amount_icp_e8s: 10 * E8,
                    },
                    sns_swap_pb::CfNeuron {
                        nns_neuron_id: 3,
                        amount_icp_e8s: 30 * E8,
                    },
                ],
            },
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_2.to_string(),
                cf_neurons: vec![sns_swap_pb::CfNeuron {
                    nns_neuron_id: 2,
                    amount_icp_e8s: 20 * E8,
                }],
            },
        ];
        expected_cf_neurons.sort_by(|n1, n2| n1.hotkey_principal.cmp(&n2.hotkey_principal));
        assert_eq!(observed_cf_neurons, expected_cf_neurons);

        assert_eq!(
            id_to_neuron,
            craft_id_to_neuron(&[
                // CF neurons less 10% of their maturity.
                (90 * E8, *PRINCIPAL_ID_1, Some(1)),
                (180 * E8, *PRINCIPAL_ID_2, Some(1)),
                (270 * E8, *PRINCIPAL_ID_1, Some(1)),
                // non-CF neurons remain untouched.
                (400 * E8, *PRINCIPAL_ID_1, None),
                (500 * E8, *PRINCIPAL_ID_2, None),
            ]),
        );

        assert_clean_refund(&mut id_to_neuron, &observed_cf_neurons, &ID_TO_NEURON);
    }

    #[test]
    fn draw_funds_from_the_community_fund_cf_shrank_during_voting_period() {
        let mut id_to_neuron = ID_TO_NEURON.clone();

        let observed_cf_neurons = draw_funds_from_the_community_fund(
            &mut id_to_neuron,
            2 * *ORIGINAL_TOTAL_COMMUNITY_FUND_MATURITY_E8S_EQUIVALENT,
            /* withdrawal_amount_e8s = */ 60 * E8,
            &PARAMS,
        );

        // Inspect results.

        let mut expected_cf_neurons = vec![
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_1.to_string(),
                cf_neurons: vec![
                    sns_swap_pb::CfNeuron {
                        nns_neuron_id: 1,
                        amount_icp_e8s: 5 * E8,
                    },
                    sns_swap_pb::CfNeuron {
                        nns_neuron_id: 3,
                        amount_icp_e8s: 15 * E8,
                    },
                ],
            },
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_2.to_string(),
                cf_neurons: vec![sns_swap_pb::CfNeuron {
                    nns_neuron_id: 2,
                    amount_icp_e8s: 10 * E8,
                }],
            },
        ];
        expected_cf_neurons.sort_by(|n1, n2| n1.hotkey_principal.cmp(&n2.hotkey_principal));
        assert_eq!(observed_cf_neurons, expected_cf_neurons);

        assert_eq!(
            id_to_neuron,
            craft_id_to_neuron(&[
                // CF neurons less 10% of their maturity.
                (95 * E8, *PRINCIPAL_ID_1, Some(1)),
                (190 * E8, *PRINCIPAL_ID_2, Some(1)),
                (285 * E8, *PRINCIPAL_ID_1, Some(1)),
                // non-CF neurons remain untouched.
                (400 * E8, *PRINCIPAL_ID_1, None),
                (500 * E8, *PRINCIPAL_ID_2, None),
            ]),
        );

        assert_clean_refund(&mut id_to_neuron, &observed_cf_neurons, &ID_TO_NEURON);
    }

    #[test]
    fn draw_funds_from_the_community_fund_cf_grew_during_voting_period() {
        let mut id_to_neuron = ID_TO_NEURON.clone();

        let observed_cf_neurons = draw_funds_from_the_community_fund(
            &mut id_to_neuron,
            *ORIGINAL_TOTAL_COMMUNITY_FUND_MATURITY_E8S_EQUIVALENT / 2,
            /* withdrawal_amount_e8s = */ 60 * E8,
            &PARAMS,
        );

        // Inspect results. Same as typical (copy n' pasted).

        let mut expected_cf_neurons = vec![
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_1.to_string(),
                cf_neurons: vec![
                    sns_swap_pb::CfNeuron {
                        nns_neuron_id: 1,
                        amount_icp_e8s: 10 * E8,
                    },
                    sns_swap_pb::CfNeuron {
                        nns_neuron_id: 3,
                        amount_icp_e8s: 30 * E8,
                    },
                ],
            },
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_2.to_string(),
                cf_neurons: vec![sns_swap_pb::CfNeuron {
                    nns_neuron_id: 2,
                    amount_icp_e8s: 20 * E8,
                }],
            },
        ];
        expected_cf_neurons.sort_by(|n1, n2| n1.hotkey_principal.cmp(&n2.hotkey_principal));
        assert_eq!(observed_cf_neurons, expected_cf_neurons);

        assert_eq!(
            id_to_neuron,
            craft_id_to_neuron(&[
                // CF neurons less 10% of their maturity.
                (90 * E8, *PRINCIPAL_ID_1, Some(1)),
                (180 * E8, *PRINCIPAL_ID_2, Some(1)),
                (270 * E8, *PRINCIPAL_ID_1, Some(1)),
                // non-CF neurons remain untouched.
                (400 * E8, *PRINCIPAL_ID_1, None),
                (500 * E8, *PRINCIPAL_ID_2, None),
            ]),
        );

        assert_clean_refund(&mut id_to_neuron, &observed_cf_neurons, &ID_TO_NEURON);
    }

    #[test]
    fn draw_funds_from_the_community_fund_trivial() {
        let mut id_to_neuron = hashmap! {};
        let original_total_community_fund_maturity_e8s_equivalent = 0;

        let observed_cf_neurons = draw_funds_from_the_community_fund(
            &mut id_to_neuron,
            original_total_community_fund_maturity_e8s_equivalent,
            /* withdrawal_amount_e8s = */ 60,
            &PARAMS,
        );

        // Inspect results.
        assert_eq!(observed_cf_neurons, vec![]);
        assert_eq!(id_to_neuron, hashmap! {});

        assert_clean_refund(&mut id_to_neuron, &observed_cf_neurons, &hashmap! {});
    }

    #[test]
    fn draw_funds_from_the_community_fund_cf_not_large_enough() {
        let mut id_to_neuron = ID_TO_NEURON.clone();

        let observed_cf_neurons = draw_funds_from_the_community_fund(
            &mut id_to_neuron,
            *ORIGINAL_TOTAL_COMMUNITY_FUND_MATURITY_E8S_EQUIVALENT,
            /* withdrawal_amount_e8s = */ 1000 * E8,
            &PARAMS,
        );

        // Inspect results.

        let mut expected_cf_neurons = vec![
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_1.to_string(),
                cf_neurons: vec![
                    sns_swap_pb::CfNeuron {
                        nns_neuron_id: 1,
                        amount_icp_e8s: 100 * E8,
                    },
                    sns_swap_pb::CfNeuron {
                        nns_neuron_id: 3,
                        amount_icp_e8s: 300 * E8,
                    },
                ],
            },
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_2.to_string(),
                cf_neurons: vec![sns_swap_pb::CfNeuron {
                    nns_neuron_id: 2,
                    amount_icp_e8s: 200 * E8,
                }],
            },
        ];
        expected_cf_neurons.sort_by(|n1, n2| n1.hotkey_principal.cmp(&n2.hotkey_principal));
        assert_eq!(observed_cf_neurons, expected_cf_neurons);

        assert_eq!(
            id_to_neuron,
            craft_id_to_neuron(&[
                // CF neurons have been completely depleted.
                (0, *PRINCIPAL_ID_1, Some(1)),
                (0, *PRINCIPAL_ID_2, Some(1)),
                (0, *PRINCIPAL_ID_1, Some(1)),
                // non-CF neurons remain untouched.
                (400 * E8, *PRINCIPAL_ID_1, None),
                (500 * E8, *PRINCIPAL_ID_2, None),
            ]),
        );

        assert_clean_refund(&mut id_to_neuron, &observed_cf_neurons, &ID_TO_NEURON);
    }

    #[test]
    fn draw_funds_from_the_community_fund_exclude_small_cf_neuron_and_cap_large() {
        let params = sns_swap_pb::Params {
            min_participant_icp_e8s: 150 * E8,
            max_participant_icp_e8s: 225 * E8,
            ..PARAMS.clone()
        };
        let mut id_to_neuron = ID_TO_NEURON.clone();

        let observed_cf_neurons = draw_funds_from_the_community_fund(
            &mut id_to_neuron,
            *ORIGINAL_TOTAL_COMMUNITY_FUND_MATURITY_E8S_EQUIVALENT,
            /* withdrawal_amount_e8s = */ 600 * E8,
            &params,
        );

        // Inspect results.

        let mut expected_cf_neurons = vec![
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_1.to_string(),
                cf_neurons: vec![sns_swap_pb::CfNeuron {
                    nns_neuron_id: 3,
                    amount_icp_e8s: 225 * E8,
                }],
            },
            sns_swap_pb::CfParticipant {
                hotkey_principal: PRINCIPAL_ID_2.to_string(),
                cf_neurons: vec![sns_swap_pb::CfNeuron {
                    nns_neuron_id: 2,
                    amount_icp_e8s: 200 * E8,
                }],
            },
        ];
        expected_cf_neurons.sort_by(|n1, n2| n1.hotkey_principal.cmp(&n2.hotkey_principal));
        assert_eq!(observed_cf_neurons, expected_cf_neurons);

        assert_eq!(
            id_to_neuron,
            craft_id_to_neuron(&[
                // CF neurons.
                (100 * E8, *PRINCIPAL_ID_1, Some(1)), // Does not participate, because too small.
                (0, *PRINCIPAL_ID_2, Some(1)),        // Fully participates.
                (75 * E8, *PRINCIPAL_ID_1, Some(1)), // Participates up to the allowed participant max.
                // non-CF neurons remain untouched.
                (400 * E8, *PRINCIPAL_ID_1, None),
                (500 * E8, *PRINCIPAL_ID_2, None),
            ]),
        );

        assert_clean_refund(&mut id_to_neuron, &observed_cf_neurons, &ID_TO_NEURON);
    }

    #[test]
    fn protect_not_concluded_open_sns_token_swap_proposal_from_gc() {
        let now_seconds = 1661731390;
        let voting_period_seconds = ONE_DAY_SECONDS;
        let mut subject = ProposalData {
            decided_timestamp_seconds: 1, // has been decided
            reward_event_round: 1,        // has been rewarded
            proposal: Some(Proposal::default()),
            executed_timestamp_seconds: 1,
            latest_tally: Some(Tally {
                yes: 1,
                no: 0,
                total: 1,
                timestamp_seconds: now_seconds,
            }),
            ..Default::default()
        };
        assert_eq!(subject.status(), ProposalStatus::Executed);
        assert!(subject.can_be_purged(now_seconds, voting_period_seconds));

        // Modify subject slightly to make it no longer ineligible for purge.
        subject.proposal.as_mut().unwrap().action =
            Some(Action::OpenSnsTokenSwap(OpenSnsTokenSwap::default()));
        assert!(!subject.can_be_purged(now_seconds, voting_period_seconds));

        let rejected_proposal_data = ProposalData {
            latest_tally: Some(Tally {
                yes: 0,
                no: 1,
                total: 1,
                timestamp_seconds: now_seconds,
            }),
            ..subject.clone()
        };
        assert_eq!(rejected_proposal_data.status(), ProposalStatus::Rejected);
        assert!(rejected_proposal_data.can_be_purged(now_seconds, voting_period_seconds));

        // Modify again to make it purge-able.
        subject.sns_token_swap_lifecycle = Some(sns_swap_pb::Lifecycle::Aborted as i32);
        assert!(subject.can_be_purged(now_seconds, voting_period_seconds));
    }

    #[test]
    fn sum_cf_participants_e8s_nonempty() {
        assert_eq!(
            sum_cf_participants_e8s(&[
                sns_swap_pb::CfParticipant {
                    hotkey_principal: PRINCIPAL_ID_1.to_string(),
                    cf_neurons: vec![
                        sns_swap_pb::CfNeuron {
                            nns_neuron_id: 1,
                            amount_icp_e8s: 100,
                        },
                        sns_swap_pb::CfNeuron {
                            nns_neuron_id: 3,
                            amount_icp_e8s: 300,
                        },
                    ],
                },
                sns_swap_pb::CfParticipant {
                    hotkey_principal: PRINCIPAL_ID_2.to_string(),
                    cf_neurons: vec![sns_swap_pb::CfNeuron {
                        nns_neuron_id: 2,
                        amount_icp_e8s: 200,
                    }],
                },
            ]),
            600,
        );
    }

    mod settle_community_fund_participation_tests {
        use super::*;
        use settle_community_fund_participation::{Aborted, Committed, Result};

        lazy_static! {
            static ref COMMITTED: SettleCommunityFundParticipation =
                SettleCommunityFundParticipation {
                    open_sns_token_swap_proposal_id: Some(7),
                    result: Some(Result::Committed(Committed {
                        sns_governance_canister_id: Some(PrincipalId::new_user_test_id(672891)),
                    })),
                };
            static ref ABORTED: SettleCommunityFundParticipation =
                SettleCommunityFundParticipation {
                    open_sns_token_swap_proposal_id: Some(42),
                    result: Some(Result::Aborted(Aborted {})),
                };
        }

        #[test]
        fn ok() {
            assert_is_ok!(validate_settle_community_fund_participation(&COMMITTED));
            assert_is_ok!(validate_settle_community_fund_participation(&ABORTED));
        }

        #[test]
        fn no_proposal_id() {
            assert_is_err!(validate_settle_community_fund_participation(
                &SettleCommunityFundParticipation {
                    open_sns_token_swap_proposal_id: None,
                    ..COMMITTED.clone()
                }
            ));
        }

        #[test]
        fn no_result() {
            assert_is_err!(validate_settle_community_fund_participation(
                &SettleCommunityFundParticipation {
                    result: None,
                    ..COMMITTED.clone()
                }
            ));
        }

        #[test]
        fn no_sns_governance_canister_id() {
            assert_is_err!(validate_settle_community_fund_participation(
                &SettleCommunityFundParticipation {
                    open_sns_token_swap_proposal_id: Some(7),
                    result: Some(Result::Committed(Committed {
                        sns_governance_canister_id: None,
                    })),
                }
            ));
        }
    }
}
