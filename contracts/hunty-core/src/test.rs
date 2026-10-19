
use crate::HuntyCore;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{Address, Env, String};

/// Helper to execute contract operations within the contract context.
/// Wraps calls with `env.as_contract()` for proper storage isolation.
fn execute_in_contract<T, F>(env: &Env, contract_id: &Address, f: F) -> T
where
    F: FnOnce(&Env) -> T,
{
    env.as_contract(contract_id, || f(env))
}
#[cfg(test)]
extern crate std;

use std::format;
use std::string::ToString;

#[cfg(test)]
mod test {
    // Benchmark-style micro tests (best-effort gas/footprint proxy)

    use super::*;
    use crate::ANSWER_SUBMISSION_WINDOW_SECS;
    use crate::MIN_HUNT_DURATION;
    use soroban_sdk::{Address, Env, IntoVal, String, Symbol, TryIntoVal, Vec};
    // Bring Soroban testutils traits into scope (generate addresses, set ledger info, register contracts).
    use crate::errors::{HuntError, HuntErrorCode};
    use crate::storage::Storage;
    use crate::types::{
        BatchClueInput, ClueAddedEvent, ClueInfo, CreatorBlacklistedEvent,
        CreatorRemovedFromBlacklistEvent, HuntCancelledEvent, HuntClosedEvent, HuntCompletedEvent,
        HuntCreatedEvent, HuntPrivacyChangedEvent, HuntStatus, HuntStatusChangedEvent,
        LeaderboardResult, PlayerProgress, PlayerRegisteredEvent, RewardClaimFailedEvent,
        TimeBonusConfig,
    };

    /// Mirrors the private production constant used for submission timestamp validation.
    const ANSWER_SUBMISSION_WINDOW_SECS: u64 = 300;
    use crate::HuntyCore;
    use nft_reward::NftReward;
    use reward_manager::RewardManager;
    use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _, Register as _};
    use soroban_sdk::{token, String as SorobanString, TryFromVal, Val};

    /// Runs a closure inside a registered HuntyCore contract context so storage is accessible.
    fn with_core_contract<T>(env: &Env, f: impl FnOnce(&Env, &Address) -> T) -> T {
        let contract_id = env.register_contract(None, super::HuntyCore);
        env.as_contract(&contract_id, || f(env, &contract_id))
    }

    fn find_hunt_status_changed_event(env: &Env) -> Option<HuntStatusChangedEvent> {
        let expected_topic: Val = Symbol::new(env, "HuntStatusChanged").into_val(env);
        let events = env.events().all();
        let mut idx = 0;
        while idx < events.len() {
            let event = events.get(idx).unwrap();
            let topics = &event.1;
            if topics.len() > 0 {
                let topic = topics.get(0).unwrap();
                if topic.get_payload() == expected_topic.get_payload() {
                    return HuntStatusChangedEvent::try_from_val(env, &event.2).ok();
                }
            }
            idx += 1;
        }
        None
    }

    fn find_hunt_status_event_for_hunt(
        env: &Env,
        hunt_id: u64,
        new_status: HuntStatus,
    ) -> Option<HuntStatusChangedEvent> {
        let expected_topic: Val = Symbol::new(env, "HuntStatusChanged").into_val(env);
        let events = env.events().all();
        let mut idx = 0;
        while idx < events.len() {
            let event = events.get(idx).unwrap();
            let topics = &event.1;
            if topics.len() > 0 {
                let topic = topics.get(0).unwrap();
                if topic.get_payload() == expected_topic.get_payload() {
                    if let Ok(status_event) = HuntStatusChangedEvent::try_from_val(env, &event.2) {
                        if status_event.hunt_id == hunt_id && status_event.new_status == new_status {
                            return Some(status_event);
                        }
                    }
                }
            }
            idx += 1;
        }
        None
    }

    fn find_event<T: TryFromVal<Env, Val>>(env: &Env, topic_name: &str) -> Option<(Vec<Val>, T)> {
        let expected_topic: Val = Symbol::new(env, topic_name).into_val(env);
        let events = env.events().all();
        let mut idx = 0;
        while idx < events.len() {
            let event = events.get(idx).unwrap();
            let topics = event.1.clone();
            if topics.len() > 0
                && topics.get(0).unwrap().get_payload() == expected_topic.get_payload()
            {
                if let Ok(data) = T::try_from_val(env, &event.2) {
                    return Some((topics, data));
                }
            }
            idx += 1;
        }
        None
    }

    /// Runs a closure in the given contract's context. Use when multiple invocations must share
    /// the same storage; call once per step that uses require_auth (Soroban allows one auth per frame).
    fn as_core_contract<T>(env: &Env, contract_id: &Address, f: impl FnOnce(&Env) -> T) -> T {
        env.as_contract(contract_id, || f(env))
    }

    #[test]
    fn test_creator_daily_rate_limit_keeps_single_entry() {
        let env = Env::default();
        let creator = Address::generate(&env);
        let contract_id = env.register(HuntyCore, ());
        let days = [1_700_000_000u64, 1_700_086_400, 1_700_172_800];
        let titles = [
            "Rate Limit Day 1",
            "Rate Limit Day 2",
            "Rate Limit Day 3",
        ];

        for (i, timestamp) in days.iter().enumerate() {
            env.ledger().set_timestamp(*timestamp);
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    String::from_str(env, titles[i]),
                    String::from_str(env, "Verify one storage entry per creator"),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
            });
        }

        let day1 = days[0] / 86_400;
        let day2 = days[1] / 86_400;
        let day3 = days[2] / 86_400;

        // RateLimiter::check_and_increment (called from create_hunt) stores a
        // single RateLimitData entry per creator under the namespaced persistent
        // key (Symbol("HRATE"), Address). Old per-day keys and bare-address keys
        // must not exist.

        use crate::rate_limit::RateLimitData;
        let namespaced_key = (Symbol::new(&env, "HRATE"), creator.clone());

        // Old triple-tuple per-day keys must not exist.
        let prefix = Symbol::new(&env, "CreatorDailyHuntCount");
        for day in [day1, day2, day3] {
            let old_key = (prefix.clone(), creator.clone(), day);
            assert!(
                !env.storage().persistent().has(&old_key),
                "rate-limit storage must not retain a per-day entry for day {}",
                day
            );
        }

        // Bare address key must not exist.
        assert!(
            !env.storage().persistent().has(&creator),
            "rate-limit must not be stored under bare Address key"
        );

        // Exactly one namespaced entry exists, holding the last day and count = 1.
        let entry: RateLimitData = env
            .storage()
            .persistent()
            .get(&namespaced_key)
            .expect("rate-limit entry must exist under (HRATE, Address)");
        assert_eq!(entry.day, day3, "stored day should be the last day");
        assert_eq!(entry.count, 1, "count should be 1 (one hunt created on day3)");
    }

    #[test]
    fn test_daily_rate_limit_rolling_window_blocks_across_midnight() {
        let env = Env::default();
        let creator = Address::generate(&env);
        let contract_id = env.register(HuntyCore, ());
        // Choose a timestamp at 23:59:59 UTC on a known day.
        let just_before_midnight = 1_700_000_000u64 / 86_400 * 86_400 + 86_399;
        env.ledger().set_timestamp(just_before_midnight);
        env.mock_all_auths();

        // Create 10 hunts, filling the rolling window.
        for _ in 0..10 {
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    String::from_str(env, "Boundary Hunt"),
                    String::from_str(env, "Boundary test"),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
            });
        }

        // Advance 2 seconds across the UTC boundary.
        env.ledger().set_timestamp(just_before_midnight + 2);

        // The 10 hunts from the previous second are still inside the rolling 24h window.
        let result = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Extra hunt"),
                String::from_str(env, "Should be rate limited"),
                None,
                None,
                0,
                None,
                None,
            )
        });

        assert_eq!(result, Err(HuntErrorCode::RateLimitExceeded));
    }

    /// Submits an answer at the current ledger timestamp using the given replay-protection nonce.
    fn submit_answer(
        env: &Env,
        hunt_id: u64,
        clue_id: u32,
        player: Address,
        answer: String,
        submission_nonce: u64,
    ) -> Result<(), HuntErrorCode> {
        HuntyCore::submit_answer(
            env.clone(),
            hunt_id,
            clue_id,
            player,
            answer,
            submission_nonce,
            env.ledger().timestamp(),
        )
    }

    /// Helper to set up RewardManager with XLM token and optional default NFT contract.
    fn setup_reward_manager(
        env: &Env,
        nft_contract: Option<&Address>,
    ) -> (Address, Address, Address) {
        let reward_manager_id = env.register(RewardManager, ());
        let token_admin = Address::generate(env);
        let token_contract = env.register_stellar_asset_contract_v2(token_admin.clone());
        let token_address = token_contract.address();

        env.as_contract(&reward_manager_id, || {
            RewardManager::initialize(env.clone(), token_admin.clone(), token_address.clone())
                .unwrap();
        });
        if let Some(nft) = nft_contract {
            env.mock_all_auths();
            env.as_contract(&reward_manager_id, || {
                RewardManager::set_nft_reward_contract(
                    env.clone(),
                    token_admin.clone(),
                    nft.clone(),
                )
                .unwrap();
            });
        }

        (reward_manager_id, token_address, token_admin)
    }

    fn submit_answer(
        env: &Env,
        hunt_id: u64,
        clue_id: u32,
        player: Address,
        answer: String,
        nonce: u64,
    ) -> Result<bool, HuntErrorCode> {
        let now = env.ledger().timestamp();
        HuntyCore::submit_answer(env.clone(), hunt_id, clue_id, player, answer, nonce, now)
    }

    #[test]
    fn test_error_with_context_display() {
        let err = HuntError::HuntNotFound;
        let hunt_error: HuntErrorCode = err.into();
        assert_eq!(hunt_error, HuntErrorCode::HuntNotFound)
    }

    #[test]
    fn test_correct_answers_do_not_reset_rate_limit_window() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let player = Address::generate(&env);
        let contract_id = env.register(HuntyCore, ());

        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Correct Rate Limit Hunt"),
                String::from_str(env, "Correct answers must consume rate limit budget"),
                None,
                None,
                1,
                None,
                None,
            )
            .unwrap()
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                String::from_str(env, "Question 1?"),
                String::from_str(env, "correct1"),
                10,
                true,
                None,
                None,
            )
            .unwrap();
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                String::from_str(env, "Question 2?"),
                String::from_str(env, "correct2"),
                10,
                true,
                None,
                None,
            )
            .unwrap();
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
            HuntyCore::register_player(env.clone(), hunt_id, player.clone()).unwrap();
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            submit_answer(env, hunt_id, 1, player.clone(), String::from_str(env, "correct1"), 1)
                .unwrap();
        });
        let progress = as_core_contract(&env, &contract_id, |env| {
            Storage::get_player_progress(env, hunt_id, &player).unwrap()
        });
        assert_eq!(progress.recent_submissions.len(), 1);

        env.mock_all_auths();
        let result = as_core_contract(&env, &contract_id, |env| {
            submit_answer(env, hunt_id, 2, player.clone(), String::from_str(env, "correct2"), 2)
        });
        assert_eq!(result, Err(HuntErrorCode::RateLimitExceeded));
    }

    #[test]
    fn test_all_error_codes_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        let variants: &[(HuntErrorCode, &str)] = &[
            (HuntErrorCode::HuntNotFound, "HuntNotFound"),
            (HuntErrorCode::ClueNotFound, "ClueNotFound"),
            (HuntErrorCode::InvalidHuntStatus, "InvalidHuntStatus"),
            (HuntErrorCode::PlayerNotRegistered, "PlayerNotRegistered"),
            (HuntErrorCode::ClueAlreadyCompleted, "ClueAlreadyCompleted"),
            (HuntErrorCode::InvalidAnswer, "InvalidAnswer"),
            (HuntErrorCode::HuntNotActive, "HuntNotActive"),
            (HuntErrorCode::Unauthorized, "Unauthorized"),
            (
                HuntErrorCode::InsufficientRewardPool,
                "InsufficientRewardPool",
            ),
            (
                HuntErrorCode::DuplicateRegistration,
                "DuplicateRegistration",
            ),
            (HuntErrorCode::InvalidTitle, "InvalidTitle"),
            (HuntErrorCode::InvalidDescription, "InvalidDescription"),
            (HuntErrorCode::InvalidAddress, "InvalidAddress"),
            (HuntErrorCode::TooManyClues, "TooManyClues"),
            (HuntErrorCode::InvalidQuestion, "InvalidQuestion"),
            (HuntErrorCode::RefundFailed, "RefundFailed"),
            (HuntErrorCode::NoCluesAdded, "NoCluesAdded"),
            (HuntErrorCode::HuntNotCompleted, "HuntNotCompleted"),
            (HuntErrorCode::RewardAlreadyClaimed, "RewardAlreadyClaimed"),
            (
                HuntErrorCode::RewardDistributionFailed,
                "RewardDistributionFailed",
            ),
            (HuntErrorCode::NoRewardsConfigured, "NoRewardsConfigured"),
            (HuntErrorCode::DuplicateSubmission, "DuplicateSubmission"),
            (HuntErrorCode::SubmissionExpired, "SubmissionExpired"),
            (HuntErrorCode::BannedPlayer, "BannedPlayer"),
            (HuntErrorCode::NoRequiredClues, "NoRequiredClues"),
            (HuntErrorCode::RateLimitExceeded, "RateLimitExceeded"),
            (HuntErrorCode::ScoreOverflow, "ScoreOverflow"),
            (HuntErrorCode::RegistrationsPaused, "RegistrationsPaused"),
            (HuntErrorCode::AnswersPaused, "AnswersPaused"),
            (HuntErrorCode::RewardsPaused, "RewardsPaused"),
            (HuntErrorCode::HuntEndTimeInPast, "HuntEndTimeInPast"),
            (HuntErrorCode::NoPendingAdmin, "NoPendingAdmin"),
            (HuntErrorCode::PendingAdminMismatch, "PendingAdminMismatch"),
            (HuntErrorCode::InvalidRarity, "InvalidRarity"),
            (
                HuntErrorCode::InvalidTimeBonusConfig,
                "InvalidTimeBonusConfig",
            ),
            (HuntErrorCode::AddressBlacklisted, "AddressBlacklisted"),
            (HuntErrorCode::ContractPaused, "ContractPaused"),
        ];
        for (variant, name) in variants {
            let code = *variant as u32;
            assert!(
                seen.insert(code),
                "Duplicate HuntErrorCode value {} for variant '{}'",
                code,
                name
            );
        }
    }

    #[test]
    fn test_hunt_not_found_converts_to_code() {
        let err = HuntError::HuntNotFound;
        let code: HuntErrorCode = err.into();
        assert_eq!(code, HuntErrorCode::HuntNotFound);
    }

    #[test]
    fn test_issue_686_error_variants_convert_to_codes() {
        let cases = [
            (HuntError::RefundFailed, HuntErrorCode::RefundFailed),
            (HuntError::NoCluesAdded, HuntErrorCode::NoCluesAdded),
            (
                HuntError::InvalidMaxAttempts,
                HuntErrorCode::InvalidMaxAttempts,
            ),
        ];

        for (error, expected_code) in cases {
            let code: HuntErrorCode = error.into();
            assert_eq!(code, expected_code);
        }
    }

    #[test]
    fn test_clue_not_found_converts_to_code() {
        let err = HuntError::ClueNotFound;
        let code: HuntErrorCode = err.into();
        assert_eq!(code, HuntErrorCode::ClueNotFound);
    }

    #[test]
    fn test_public_answer_hash_cannot_be_replayed_as_an_answer() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let player1 = Address::generate(&env);
        let player2 = Address::generate(&env);
        let contract_id = env.register(HuntyCore, ());

        // Create hunt
        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Hash Hunt"),
                String::from_str(env, "Test hashing paths"),
                None,
                None,
                0,
                None,
                None,
            )
        })
        .unwrap();

        // Add a clue with answer "Paris"
        env.mock_all_auths();
        let clue_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                String::from_str(env, "Capital of France?"),
                String::from_str(env, "Paris"),
                10,
                true,
                None,
            )
        })
        .unwrap();

        // Register two players
        env.as_contract(&contract_id, || {
            HuntyCore::register_player(env.clone(), hunt_id, player1.clone()).unwrap();
        });
        env.as_contract(&contract_id, || {
            HuntyCore::register_player(env.clone(), hunt_id, player2.clone()).unwrap();
        });

        // Read the value exposed in the stored Clue, as a ledger reader could.
        let leaked_digest_text = env.as_contract(&contract_id, || {
            let clue = Storage::get_clue(&env, hunt_id, clue_id).unwrap();
            let digest = clue.answer_hashes.get(0).unwrap().to_array();
            let mut hex = Bytes::new(&env);
            for byte in digest {
                hex.push_back(b"0123456789abcdef"[(byte >> 4) as usize]);
                hex.push_back(b"0123456789abcdef"[(byte & 0x0f) as usize]);
            }
            String::from_bytes(&env, &hex.to_array())
        });

        // Submitting the public digest as plaintext does not reveal its preimage.
        let replay = env.as_contract(&contract_id, || {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                clue_id,
                player2.clone(),
                leaked_digest_text,
                1,
                env.ledger().timestamp(),
            )
        });
        assert_eq!(replay, Err(HuntErrorCode::InvalidAnswer));

        let progress = env.as_contract(&contract_id, || {
            Storage::get_player_progress(&env, hunt_id, &player2).unwrap()
        });
        assert!(!progress.has_completed_clue(clue_id));
    }

    #[test]
    fn test_hunt_completion_ranks() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let player1 = Address::generate(&env);
        let player2 = Address::generate(&env);
        let player3 = Address::generate(&env);
        let contract_id = env.register(HuntyCore, ());

        // Create hunt
        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Rank Hunt"),
                String::from_str(env, "Test ranking"),
                None,
                None,
                0,
                None,
                None,
            )
        })
        .unwrap();

        let question = String::from_str(&env, "What is 2+2?");
        let answer = String::from_str(&env, "4");
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                question.clone(),
                answer.clone(),
                10,
                true,
                None,
                None,
            )
            .unwrap();
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::register_player(env.clone(), hunt_id, player1.clone()).unwrap();
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::register_player(env.clone(), hunt_id, player2.clone()).unwrap();
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::register_player(env.clone(), hunt_id, player3.clone()).unwrap();
        });

        // Player1 completes
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            submit_answer(env, hunt_id, 1, player1.clone(), answer.clone(), 1).unwrap();
        });
        let board1 = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::get_hunt_leaderboard(env.clone(), hunt_id, 10)
                .unwrap()
                .entries
        });
        let first = board1.get(0).unwrap();
        assert_eq!(first.player, player1);
        assert_eq!(first.rank, 1);
        assert!(first.is_completed);

        // Player2 completes
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            submit_answer(env, hunt_id, 1, player2.clone(), answer.clone(), 2).unwrap();
        });
        let board2 = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::get_hunt_leaderboard(env.clone(), hunt_id, 10)
                .unwrap()
                .entries
        });
        let first_after_second = board2.get(0).unwrap();
        let second_after_second = board2.get(1).unwrap();
        assert_eq!(first_after_second.player, player1);
        assert_eq!(first_after_second.rank, 1);
        assert_eq!(second_after_second.player, player2);
        assert_eq!(second_after_second.rank, 2);
        assert!(second_after_second.is_completed);

        // Duplicate attempt by Player2 (should not emit new event)
        env.mock_all_auths();
        let dup_result = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player2.clone(),
                answer.clone(),
                2,
                env.ledger().timestamp(),
            )
        });
        assert_eq!(dup_result, Err(HuntErrorCode::DuplicateSubmission));
        let board_dup = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::get_hunt_leaderboard(env.clone(), hunt_id, 10)
                .unwrap()
                .entries
        });
        let first_after_dup = board_dup.get(0).unwrap();
        let second_after_dup = board_dup.get(1).unwrap();
        assert_eq!(first_after_dup.player, player1);
        assert_eq!(first_after_dup.rank, 1);
        assert_eq!(second_after_dup.player, player2);
        assert_eq!(second_after_dup.rank, 2);
    }

    #[test]
    fn test_submit_answer_rejects_expired_submission_timestamp() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let player = Address::generate(&env);
        let question = String::from_str(&env, "What is 2+2?");
        let answer = String::from_str(&env, "4");

        let contract_id = env.register(HuntyCore, ());
        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Replay Hunt"),
                String::from_str(env, "Replay protection"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap()
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                question.clone(),
                answer.clone(),
                10,
                true,
                None,
                None,
            )
            .unwrap();
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::register_player(env.clone(), hunt_id, player.clone()).unwrap();
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            let result = HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player.clone(),
                answer.clone(),
                1,
                env.ledger().timestamp() - ANSWER_SUBMISSION_WINDOW_SECS - 1,
            );
            assert_eq!(result, Err(HuntErrorCode::SubmissionExpired));
        });
    }

    #[test]
    fn test_wrong_answers_consume_rate_limit_budget() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let player = Address::generate(&env);
        let contract_id = env.register(HuntyCore, ());

        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Rate Limited Hunt"),
                String::from_str(env, "Wrong answers must consume rate limit budget"),
                None,
                None,
                3,
                None,
                None,
            )
            .unwrap()
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                String::from_str(env, "Question?"),
                String::from_str(env, "correct"),
                10,
                true,
                None,
                None,
            )
            .unwrap();
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
            HuntyCore::register_player(env.clone(), hunt_id, player.clone()).unwrap();
        });

        for nonce in 1..=3 {
            env.mock_all_auths();
            let result = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer(
                    env.clone(),
                    hunt_id,
                    1,
                    player.clone(),
                    String::from_str(env, "wrong"),
                    nonce,
                    env.ledger().timestamp(),
                )
            });
            assert_eq!(result, Err(HuntErrorCode::InvalidAnswer));
        }

        let progress = as_core_contract(&env, &contract_id, |env| {
            Storage::get_player_progress(env, hunt_id, &player).unwrap()
        });
        assert_eq!(progress.recent_submissions.len(), 3);

        env.mock_all_auths();
        let result = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player.clone(),
                String::from_str(env, "wrong"),
                4,
                env.ledger().timestamp(),
            )
        });
        assert_eq!(result, Err(HuntErrorCode::RateLimitExceeded));
    }

    #[test]
    #[should_panic]
    fn test_set_max_attempts_requires_creator_auth_even_when_caller_is_forged() {
        let env = Env::default();
        let creator = Address::generate(&env);
        let contract_id = env.register(HuntyCore, ());

        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Attempt Limit Hunt"),
                String::from_str(env, "Creator authorization required"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap()
        });

        env.set_auths(&[]);
        as_core_contract(&env, &contract_id, |env| {
            let _ = HuntyCore::set_max_attempts_per_clue(
                env.clone(),
                hunt_id,
                creator.clone(),
                9,
                0,
            );
        });
    }

    #[test]
    fn test_max_attempts_are_enforced_per_clue() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let player = Address::generate(&env);
        let contract_id = env.register(HuntyCore, ());

        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Attempt Limit Hunt"),
                String::from_str(env, "Attempts are limited per clue"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap()
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                String::from_str(env, "First clue?"),
                String::from_str(env, "first answer"),
                10,
                true,
                None,
                None,
            )
            .unwrap();
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                String::from_str(env, "Second clue?"),
                String::from_str(env, "second answer"),
                10,
                true,
                None,
                None,
            )
            .unwrap();
            HuntyCore::set_max_attempts_per_clue(env.clone(), hunt_id, creator.clone(), 2, 0)
                .unwrap();
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
            HuntyCore::register_player(env.clone(), hunt_id, player.clone()).unwrap();
        });

        for nonce in 1..=2 {
            env.mock_all_auths();
            let result = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer(
                    env.clone(),
                    hunt_id,
                    1,
                    player.clone(),
                    String::from_str(env, "wrong answer"),
                    nonce,
                    env.ledger().timestamp(),
                )
            });
            assert_eq!(result, Err(HuntErrorCode::InvalidAnswer));
        }

        env.mock_all_auths();
        let exhausted = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player.clone(),
                String::from_str(env, "first answer"),
                3,
                env.ledger().timestamp(),
            )
        });
        assert_eq!(exhausted, Err(HuntErrorCode::InvalidMaxAttempts));

        env.mock_all_auths();
        let other_clue = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                2,
                player.clone(),
                String::from_str(env, "second answer"),
                4,
                env.ledger().timestamp(),
            )
        });
        assert_eq!(other_clue, Ok(()));
    }

    #[test]
    fn test_hunt_created_event_topics_and_data() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let title = String::from_str(&env, "Indexed Hunt");

        with_core_contract(&env, |env, _cid| {
            env.mock_all_auths();
            let hunt_id = HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                title.clone(),
                String::from_str(env, "Event payload coverage"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap();

            let (topics, event) =
                find_event::<HuntCreatedEvent>(env, "HuntCreated").expect("missing HuntCreated");
            assert_eq!(topics.len(), 2);
            assert_eq!(
                topics.get(0).unwrap().get_payload(),
                Symbol::new(env, "HuntCreated").into_val(env).get_payload()
            );
            assert_eq!(
                topics.get(1).unwrap().get_payload(),
                hunt_id.into_val(env).get_payload()
            );
            assert_eq!(event.hunt_id, hunt_id);
            assert_eq!(event.creator, creator);
        });
    }

    #[test]
    fn test_clue_added_event_topics_and_data() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let question = String::from_str(&env, "What walks on four legs?");

        let contract_id = env.register(HuntyCore, ());
        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Clue Event Hunt"),
                String::from_str(env, "Verifies indexed clue metadata"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap()
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            let clue_id = HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                question.clone(),
                String::from_str(env, "Human"),
                25,
                true,
                Some(3),
                None,
            )
            .unwrap();

            let (topics, event) =
                find_event::<ClueAddedEvent>(env, "ClueAdded").expect("missing ClueAdded");
            assert_eq!(topics.len(), 3);
            assert_eq!(
                topics.get(0).unwrap().get_payload(),
                Symbol::new(env, "ClueAdded").into_val(env).get_payload()
            );
            assert_eq!(
                topics.get(1).unwrap().get_payload(),
                hunt_id.into_val(env).get_payload()
            );
            assert_eq!(
                topics.get(2).unwrap().get_payload(),
                clue_id.into_val(env).get_payload()
            );
            assert_eq!(event.hunt_id, hunt_id);
            assert_eq!(event.clue_id, clue_id);
            assert_eq!(event.creator, creator);
            assert_eq!(event.question, question);
            assert_eq!(event.points, 25);
            assert!(event.is_required);
        });
    }

    #[test]
    fn test_player_registered_event_topics_and_data() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let player = Address::generate(&env);

        let contract_id = env.register(HuntyCore, ());
        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            let hunt_id = HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Registration Event Hunt"),
                String::from_str(env, "Verifies player registration indexing"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap();
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                question.clone(),
                answer.clone(),
                10,
                true,
                None,
            )
            .unwrap();
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
            hunt_id
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::register_player(env.clone(), hunt_id, player.clone()).unwrap();

            let (topics, event) = find_event::<PlayerRegisteredEvent>(env, "PlayerRegistered")
                .expect("missing PlayerRegistered");
            assert_eq!(topics.len(), 2);
            assert_eq!(
                topics.get(0).unwrap().get_payload(),
                Symbol::new(env, "PlayerRegistered")
                    .into_val(env)
                    .get_payload()
            );
            assert_eq!(
                topics.get(1).unwrap().get_payload(),
                hunt_id.into_val(env).get_payload()
            );
            assert_eq!(event.hunt_id, hunt_id);
            assert_eq!(event.player, player);
        });
    }

    #[test]
    fn test_processed_submission_tracking_expires_after_window() {
        let env = Env::default();
        let start_time = 1_700_000_000;
        env.ledger().set_timestamp(start_time);
        let creator = Address::generate(&env);
        let player = Address::generate(&env);
        let question = String::from_str(&env, "What is 2+2?");
        let answer = String::from_str(&env, "4");

        let contract_id = env.register(HuntyCore, ());
        env.mock_all_auths();
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Replay Hunt"),
                String::from_str(env, "Replay protection"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap()
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                question.clone(),
                answer.clone(),
                10,
                true,
                None,
                None,
            )
            .unwrap();
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
        });
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::register_player(env.clone(), hunt_id, player.clone()).unwrap();
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            let submitted_at = env.ledger().timestamp();
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player.clone(),
                answer.clone(),
                7,
                submitted_at,
            )
            .unwrap();

            assert_eq!(
                Storage::get_processed_submission_expiry(env, hunt_id, 1, &player, 7, submitted_at,),
                Some(submitted_at + ANSWER_SUBMISSION_WINDOW_SECS)
            );

            env.ledger()
                .set_timestamp(submitted_at + ANSWER_SUBMISSION_WINDOW_SECS + 1);
            HuntyCore::assert_submission_not_replayed(
                env,
                hunt_id,
                1,
                &player,
                7,
                submitted_at,
                env.ledger().timestamp(),
            )
            .unwrap();

            assert_eq!(
                Storage::get_processed_submission_expiry(env, hunt_id, 1, &player, 7, submitted_at,),
                None
            );
        });
    }

    #[test]
    fn test_invalid_hunt_status_message() {
        let err = HuntError::InvalidHuntStatus;
        let code: HuntErrorCode = err.into();
        assert_eq!(code, HuntErrorCode::InvalidHuntStatus);
    }

    #[test]
    fn test_insufficient_reward_pool_converts_to_code() {
        let err = HuntError::InsufficientRewardPool;
        let code: HuntErrorCode = err.into();
        assert_eq!(code, HuntErrorCode::InsufficientRewardPool);
    }

    // ========== create_hunt() Tests ==========

    #[test]
    fn test_create_hunt_success() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let title = String::from_str(&env, "Test Hunt");
        let description = String::from_str(&env, "This is a test hunt description");

        let (hunt_id, hunt) = with_core_contract(&env, |env, _cid| {
            let hunt_id = HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                title.clone(),
                description.clone(),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap();
            let hunt = Storage::get_hunt(env, hunt_id).unwrap();
            (hunt_id, hunt)
        });

        // Verify hunt ID is 1 (first hunt)
        assert_eq!(hunt_id, 1);
        assert_eq!(hunt.hunt_id, hunt_id);
        assert_eq!(hunt.creator, creator);
        assert_eq!(hunt.title, title);
        assert_eq!(hunt.description, description);
        assert_eq!(hunt.status, HuntStatus::Draft);
        assert_eq!(hunt.total_clues, 0);
        assert_eq!(hunt.required_clues, 0);
        assert_eq!(hunt.reward_config.xlm_pool, 0);
        assert_eq!(hunt.reward_config.nft_enabled, false);
        assert_eq!(hunt.reward_config.max_winners, 0);
        assert_eq!(hunt.reward_config.claimed_count, 0);
        assert_eq!(hunt.time_bonus_start_bps, None);
        assert_eq!(hunt.time_bonus_min_bps, None);
        assert_eq!(hunt.time_bonus_decay_secs, None);
        assert!(hunt.created_at > 0);
        assert_eq!(hunt.activated_at, 0);
        assert_eq!(hunt.end_time, 0);
    }

    #[test]
    fn test_time_bonus_scoring_decreases_over_time() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);

        let creator = Address::generate(&env);
        let player_fast = Address::generate(&env);
        let player_mid = Address::generate(&env);
        let player_slow = Address::generate(&env);
        let title = String::from_str(&env, "Time Bonus Hunt");
        let description = String::from_str(&env, "A hunt with a decaying score bonus");
        let question = String::from_str(&env, "What time is it?");
        let answer = String::from_str(&env, "now");
        let bonus = TimeBonusConfig {
            start_multiplier_bps: 20_000,
            min_multiplier_bps: 10_000,
            decay_duration_secs: 100,
        };

        let contract_id = env.register_contract(None, super::HuntyCore);
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                title.clone(),
                description.clone(),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap()
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::set_time_bonus_config(
                env.clone(),
                hunt_id,
                creator.clone(),
                Some(bonus.clone()),
            )
            .unwrap();
            let hunt = Storage::get_hunt(env, hunt_id).unwrap();
            assert_eq!(hunt.time_bonus_start_bps, Some(bonus.start_multiplier_bps));
            assert_eq!(hunt.time_bonus_min_bps, Some(bonus.min_multiplier_bps));
            assert_eq!(hunt.time_bonus_decay_secs, Some(bonus.decay_duration_secs));
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                question.clone(),
                answer.clone(),
                10,
                true,
                Some(1),
                None,
            )
            .unwrap();
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::register_player(env.clone(), hunt_id, player_fast.clone()).unwrap();
            HuntyCore::register_player(env.clone(), hunt_id, player_mid.clone()).unwrap();
            HuntyCore::register_player(env.clone(), hunt_id, player_slow.clone()).unwrap();
        });

        env.ledger().set_timestamp(1_700_000_000);
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player_fast.clone(),
                answer.clone(),
                1, /* nonce */
                env.ledger().timestamp(),
            )
            .unwrap();
        });

        env.ledger().set_timestamp(1_700_000_050);
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player_mid.clone(),
                answer.clone(),
                2, /* nonce */
                env.ledger().timestamp(),
            )
            .unwrap();
        });

        env.ledger().set_timestamp(1_700_000_100);
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player_slow.clone(),
                answer.clone(),
                3, /* nonce */
                env.ledger().timestamp(),
            )
            .unwrap();
        });

        let fast_progress = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::get_player_progress(env.clone(), hunt_id, player_fast.clone()).unwrap()
        });
        let mid_progress = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::get_player_progress(env.clone(), hunt_id, player_mid.clone()).unwrap()
        });
        let slow_progress = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::get_player_progress(env.clone(), hunt_id, player_slow.clone()).unwrap()
        });

        assert_eq!(fast_progress.total_score, 20);
        assert_eq!(mid_progress.total_score, 15);
        assert_eq!(slow_progress.total_score, 10);

        let board = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::get_hunt_leaderboard(env.clone(), hunt_id, 3)
                .unwrap()
                .entries
        });

        assert_eq!(board.len(), 3);
        assert_eq!(board.get(0).unwrap().player, player_fast);
        assert_eq!(board.get(0).unwrap().score, 20);
        assert_eq!(board.get(1).unwrap().player, player_mid);
        assert_eq!(board.get(1).unwrap().score, 15);
        assert_eq!(board.get(2).unwrap().player, player_slow);
        assert_eq!(board.get(2).unwrap().score, 10);
    }

    #[test]
    fn test_long_elapsed_time_bonus_clamps_and_does_not_panic() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);

        let creator = Address::generate(&env);
        let player1 = Address::generate(&env);
        let player2 = Address::generate(&env);
        let title = String::from_str(&env, "Long Run Hunt");
        let description = String::from_str(&env, "Long run time bonus clamp");
        let question = String::from_str(&env, "Question?");
        let answer = String::from_str(&env, "answer");
        let bonus = TimeBonusConfig {
            start_multiplier_bps: 20_000,
            min_multiplier_bps: 10_000,
            decay_duration_secs: 100,
        };

        let contract_id = env.register_contract(None, super::HuntyCore);
        let hunt_id = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                title,
                description,
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap()
        });

        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::set_time_bonus_config(
                env.clone(),
                hunt_id,
                creator.clone(),
                Some(bonus),
            )
            .unwrap();
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                question,
                answer.clone(),
                10,
                true,
                Some(1),
                None,
            )
            .unwrap();
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
            HuntyCore::register_player(env.clone(), hunt_id, player1.clone()).unwrap();
            HuntyCore::register_player(env.clone(), hunt_id, player2.clone()).unwrap();
        });

        // First case: decrease_bps = 4_294_970_000, just above u32::MAX.
        // `as u32` truncation kept a near-maximum multiplier; fixed code floors.
        env.ledger().set_timestamp(1_700_000_000 + 42_949_700);
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player1.clone(),
                answer.clone(),
                1,
                env.ledger().timestamp(),
            )
            .unwrap();
        });
        let progress1 = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::get_player_progress(env.clone(), hunt_id, player1.clone()).unwrap()
        });
        assert_eq!(progress1.total_score, 10);

        // Second case: elapsed = u64::MAX / 2 must saturate, not overflow.
        env.ledger().set_timestamp(1_700_000_000 + u64::MAX / 2);
        env.mock_all_auths();
        as_core_contract(&env, &contract_id, |env| {
            HuntyCore::submit_answer(
                env.clone(),
                hunt_id,
                1,
                player2.clone(),
                answer.clone(),
                1,
                env.ledger().timestamp(),
            )
            .unwrap();
        });
        let progress2 = as_core_contract(&env, &contract_id, |env| {
            HuntyCore::get_player_progress(env.clone(), hunt_id, player2).unwrap()
        });
        assert_eq!(progress2.total_score, 10);
    }

    #[test]
    fn test_create_hunt_with_end_time() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let title = String::from_str(&env, "Timed Hunt");
        let description = String::from_str(&env, "A hunt with an end time");
        let end_time = 1_700_086_400u64; // 1 day in the future

        let hunt = with_core_contract(&env, |env, _cid| {
            let hunt_id = HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                title.clone(),
                description.clone(),
                None,
                Some(end_time),
                0,
                None,
                None,
            )
            .unwrap();
            Storage::get_hunt(env, hunt_id).unwrap()
        });
        assert_eq!(hunt.end_time, end_time);
    }

    #[test]
    fn test_create_hunt_invalid_end_time() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let title = String::from_str(&env, "Expired Hunt");
        let description = String::from_str(&env, "A hunt with an expired end time");
        let end_time = 1_700_000_000; // equal to current time (invalid)

        let result = with_core_contract(&env, |env, _cid| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                title.clone(),
                description.clone(),
                None,
                Some(end_time),
                0,
                None,
                None,
            )
        });
        assert_eq!(result, Err(HuntErrorCode::HuntEndTimeInPast));

        let end_time_past = 1_699_999_999; // in the past (invalid)
        let result_past = with_core_contract(&env, |env, _cid| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                title.clone(),
                description.clone(),
                None,
                Some(end_time_past),
                0,
                None,
                None,
            )
        });
        assert_eq!(result_past, Err(HuntErrorCode::HuntEndTimeInPast));

        let end_time_too_short = 1_700_000_000 + MIN_HUNT_DURATION - 1;
        let result_too_short = with_core_contract(&env, |env, _cid| {
            HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                title.clone(),
                description.clone(),
                None,
                Some(end_time_too_short),
                0,
                None,
                None,
            )
        });
        assert_eq!(result_too_short, Err(HuntErrorCode::HuntEndTimeInPast));

        let end_time_min = 1_700_000_000 + MIN_HUNT_DURATION;
        let hunt = with_core_contract(&env, |env, _cid| {
            let hunt_id = HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                title.clone(),
                description.clone(),
                None,
                Some(end_time_min),
                0,
                None,
                None,
            )
            .unwrap();
            Storage::get_hunt(env, hunt_id).unwrap()
        });
        assert_eq!(hunt.end_time, end_time_min);
    }

    #[test]
    fn test_create_hunt_empty_title() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        let title = String::from_str(&env, "");
        let description = String::from_str(&env, "Valid description");

        let result = with_core_contract(&env, |env, _cid| {
            HuntyCore::create_hunt(
                env.clone(),
                creator,
                title,
                description,
                None,
                None,
                0,
                None,
                None,
            )
        });

        assert_eq!(result, Err(HuntErrorCode::InvalidTitle));
    }

    #[test]
    fn test_create_hunt_title_too_long() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        // Create a title longer than 200 characters
        let long_title = String::from_str(&env, &"a".repeat(201));
        let description = String::from_str(&env, "Valid description");

        let result = with_core_contract(&env, |env, _cid| {
            HuntyCore::create_hunt(
                env.clone(),
                creator,
                long_title,
                description,
                None,
                None,
                0,
                None,
                None,
            )
        });

        assert_eq!(result, Err(HuntErrorCode::InvalidTitle));
    }

    #[test]
    fn test_create_hunt_title_exactly_max_length() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        let creator = Address::generate(&env);
        // Create a title exactly 200 characters (should be valid)
        let title = String::from_str(&env, &"a".repeat(200));
        let description = String::from_str(&env, "Valid description");

        let result = with_core_contract(&env, |env, _cid| {
            HuntyCore::create_hunt(
                env.clone(),
                creator,
                title,
                description,
                None,
                None,
                0,
                None,
                None,
            )
        });

        assert!(result.is_ok());
    }

    #[test]
    fn test_add_clues_success() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        env.mock_all_auths();
        let creator = Address::generate(&env);

        let (ids, hunt, clues) = with_core_contract(&env, |env, _cid| {
            let hunt_id = HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Batch Hunt"),
                String::from_str(env, "Description"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap();
            let clues = Vec::from_array(
                env,
                [
                    BatchClueInput {
                        question: String::from_str(env, "Q1"),
                        answer: String::from_str(env, "a1"),
                        points: 10,
                        is_required: true,
                        difficulty: 1,
                        weight: None,
                    },
                    BatchClueInput {
                        question: String::from_str(env, "Q2"),
                        answer: String::from_str(env, "a2"),
                        points: 20,
                        is_required: false,
                        difficulty: 3,
                        weight: None,
                    },
                ],
            );

            let ids = HuntyCore::add_clues(env.clone(), hunt_id, clues).unwrap();
            let hunt = Storage::get_hunt(env, hunt_id).unwrap();
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
            HuntyCore::register_player(env.clone(), hunt_id, Address::generate(env)).unwrap();
            let stored = HuntyCore::list_clues(env.clone(), hunt_id, 0, 10);
            (ids, hunt, stored)
        });

        assert_eq!(ids.len(), 2);
        assert_eq!(ids.get(0).unwrap(), 1);
        assert_eq!(ids.get(1).unwrap(), 2);
        assert_eq!(hunt.total_clues, 2);
        assert_eq!(hunt.required_clues, 1);
        assert_eq!(clues.len(), 2);
        assert_eq!(clues.get(0).unwrap().points, 10);
        assert_eq!(clues.get(1).unwrap().difficulty, 3);
    }

    #[test]
    fn test_add_clues_rejects_batch_over_clue_limit() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        env.mock_all_auths();
        let creator = Address::generate(&env);

        let clue_count = with_core_contract(&env, |env, _cid| {
            let hunt_id = HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Batch Hunt"),
                String::from_str(env, "Description"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap();

            for _ in 0..99 {
                HuntyCore::add_clue(
                    env.clone(),
                    hunt_id,
                    String::from_str(env, "Q"),
                    String::from_str(env, "a"),
                    1,
                    false,
                    Some(1),
                    None,
                )
                .unwrap();
            }

            let clues = Vec::from_array(
                env,
                [
                    BatchClueInput {
                        question: String::from_str(env, "Q100"),
                        answer: String::from_str(env, "a100"),
                        points: 1,
                        is_required: false,
                        difficulty: 1,
                        weight: None,
                    },
                    BatchClueInput {
                        question: String::from_str(env, "Q101"),
                        answer: String::from_str(env, "a101"),
                        points: 1,
                        is_required: false,
                        difficulty: 1,
                        weight: None,
                    },
                ],
            );

            let err = HuntyCore::add_clues(env.clone(), hunt_id, clues).unwrap_err();
            assert_eq!(err, HuntErrorCode::TooManyClues);
            Storage::get_clue_counter(env, hunt_id)
        });

        assert_eq!(clue_count, 99);
    }

    #[test]
    fn test_add_clues_invalid_hunt_status_not_draft() {
        let env = Env::default();
        env.ledger().set_timestamp(1_700_000_000);
        env.mock_all_auths();
        let creator = Address::generate(&env);

        with_core_contract(&env, |env, _cid| {
            let hunt_id = HuntyCore::create_hunt(
                env.clone(),
                creator.clone(),
                String::from_str(env, "Batch Hunt"),
                String::from_str(env, "Description"),
                None,
                None,
                0,
                None,
                None,
            )
            .unwrap();
            HuntyCore::add_clue(
                env.clone(),
                hunt_id,
                String::from_str(env, "Required"),
                String::from_str(env, "a"),
                1,
                true,
                Some(1),
                None,
            )
            .unwrap();
            let mut hunt = Storage::get_hunt(env, hunt_id).unwrap();
            hunt.reward_config =
                crate::types::HuntRewardConfig::new(env, 100, false, None, 1, 0, 0, None);
            Storage::save_hunt(env, &hunt);
            HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();

            let clues = Vec::from_array(
                env,
                [BatchClueInput {
                    question: String::from_str(env, "Q2"),
                    answer: String::from_str(env, "a2"),
                    points: 1,
                    is_required: false,
                    difficulty: 1,
                    weight: None,
                }],
            );

            let err = HuntyCore::add_clues(env.clone(), hunt_id, clues).unwrap_err();
            assert_eq!(err, HuntErrorCode::InvalidHuntStatus);
        });
    }

    #[test]
    #[should_panic]
    fn test_add_clue_unauthorized() {
        fn test_create_hunt_description_too_long() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Valid Title");
            // Create a description longer than 2000 characters
            let long_description = String::from_str(&env, &"a".repeat(2001));

            let result = with_core_contract(&env, |env, _cid| {
                HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    long_description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
            });

            assert_eq!(result, Err(HuntErrorCode::InvalidDescription));
        }

        #[test]
        fn test_create_hunt_description_exactly_max_length() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Valid Title");
            // Create a description exactly 2000 characters (should be valid)
            let description = String::from_str(&env, &"a".repeat(2000));

            let result = with_core_contract(&env, |env, _cid| {
                HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
            });

            assert!(result.is_ok());
        }

        #[test]
        fn test_create_hunt_unique_ids() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator = Address::generate(&env);
            let title1 = String::from_str(&env, "Hunt 1");
            let title2 = String::from_str(&env, "Hunt 2");
            let title3 = String::from_str(&env, "Hunt 3");
            let description = String::from_str(&env, "Description");

            let (hunt_id1, hunt_id2, hunt_id3) = with_core_contract(&env, |env, _cid| {
                let hunt_id1 = HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title1,
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let hunt_id2 = HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title2,
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let hunt_id3 = HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title3,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                (hunt_id1, hunt_id2, hunt_id3)
            });

            // Verify IDs are unique and sequential
            assert_eq!(hunt_id1, 1);
            assert_eq!(hunt_id2, 2);
            assert_eq!(hunt_id3, 3);
            assert_ne!(hunt_id1, hunt_id2);
            assert_ne!(hunt_id2, hunt_id3);
        }

        #[test]
        fn test_create_hunt_twice_returns_different_ids() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Test Hunt");
            let description = String::from_str(&env, "Description");

            let (first_hunt_id, second_hunt_id) = with_core_contract(&env, |env, _cid| {
                let first_hunt_id = HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title.clone(),
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let second_hunt_id = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();

                (first_hunt_id, second_hunt_id)
            });

            assert_ne!(first_hunt_id, second_hunt_id);
        }

        #[test]
        fn test_create_hunt_different_creators() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator1 = Address::generate(&env);
            let creator2 = Address::generate(&env);
            let title = String::from_str(&env, "Test Hunt");
            let description = String::from_str(&env, "Description");

            let (hunt_id1, hunt_id2, hunt1, hunt2) = with_core_contract(&env, |env, _cid| {
                let hunt_id1 = HuntyCore::create_hunt(
                    env.clone(),
                    creator1.clone(),
                    title.clone(),
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let hunt_id2 = HuntyCore::create_hunt(
                    env.clone(),
                    creator2.clone(),
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let hunt1 = Storage::get_hunt(&env, hunt_id1).unwrap();
                let hunt2 = Storage::get_hunt(&env, hunt_id2).unwrap();
                (hunt_id1, hunt_id2, hunt1, hunt2)
            });

            assert_eq!(hunt1.creator, creator1);
            assert_eq!(hunt2.creator, creator2);
            assert_ne!(hunt1.creator, hunt2.creator);
        }

        #[test]
        fn test_create_hunt_counter_increments() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Test Hunt");
            let description = String::from_str(&env, "Description");

            let (start_counter, hunt_id1, counter_after_1, hunt_id2, counter_after_2, hunt_count) =
                with_core_contract(&env, |env, _cid| {
                    // Verify counter starts at 0
                    let start_counter = Storage::get_hunt_counter(env);

                    // Create first hunt
                    let hunt_id1 = HuntyCore::create_hunt(
                        env.clone(),
                        creator.clone(),
                        title.clone(),
                        description.clone(),
                        None,
                        None,
                        0,
                        None,
                        None,
                    )
                    .unwrap();

                    // Counter should be 1 after first hunt
                    let counter_after_1 = Storage::get_hunt_counter(env);

                    // Create second hunt
                    let hunt_id2 = HuntyCore::create_hunt(
                        env.clone(),
                        creator.clone(),
                        title,
                        description,
                        None,
                        None,
                        0,
                        None,
                        None,
                    )
                    .unwrap();

                    // Counter should be 2 after second hunt
                    let counter_after_2 = Storage::get_hunt_counter(env);
                    let hunt_count = HuntyCore::get_hunt_count(env.clone());

                    (
                        start_counter,
                        hunt_id1,
                        counter_after_1,
                        hunt_id2,
                        counter_after_2,
                        hunt_count,
                    )
                });

            assert_eq!(start_counter, 0);
            assert_eq!(counter_after_1, 1);
            assert_eq!(hunt_id1, 1);
            assert_eq!(counter_after_2, 2);
            assert_eq!(hunt_id2, 2);
            assert_eq!(hunt_count, 2);
        }

        #[test]
        fn test_create_hunt_default_reward_config() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Test Hunt");
            let description = String::from_str(&env, "Description");

            let hunt = with_core_contract(&env, |env, _cid| {
                let hunt_id = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                Storage::get_hunt(env, hunt_id).unwrap()
            });
            let reward_config = hunt.reward_config;

            // Verify default reward config values
            assert_eq!(reward_config.xlm_pool, 0);
            assert_eq!(reward_config.nft_enabled, false);
            assert_eq!(reward_config.nft_contract, None);
            assert_eq!(reward_config.max_winners, 0);
            assert_eq!(reward_config.claimed_count, 0);
        }

        #[test]
        fn test_create_hunt_created_at_timestamp() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Test Hunt");
            let description = String::from_str(&env, "Description");

            let (hunt, current_time) = with_core_contract(&env, |env, _cid| {
                let hunt_id = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                (
                    Storage::get_hunt(env, hunt_id).unwrap(),
                    env.ledger().timestamp(),
                )
            });

            // Created timestamp should be set and reasonable (within a few seconds)
            assert!(hunt.created_at > 0);
            assert!(hunt.created_at <= current_time);
            // Allow some small time difference for test execution
            assert!(current_time - hunt.created_at < 10);
        }

        #[test]
        fn test_create_hunt_from_template_copies_completed_hunt_clues() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let contract_id = env.register_contract(None, super::HuntyCore);

            let template_creator = Address::generate(&env);
            let new_creator = Address::generate(&env);
            let player = Address::generate(&env);
            let title = String::from_str(&env, "Template Hunt");
            let description = String::from_str(&env, "Completed hunt used as a template");
            let cloned_title = String::from_str(&env, "Remixed Hunt");
            let cloned_description = String::from_str(&env, "Fresh draft from template");
            let q1 = String::from_str(&env, "What is 2 + 2?");
            let q2 = String::from_str(&env, "What is 3 + 3?");
            let a1 = String::from_str(&env, "four");
            let a2 = String::from_str(&env, "six");

            let template_hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt(
                    env.clone(),
                    template_creator.clone(),
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap()
            });

            let mut template_hunt = as_core_contract(&env, &contract_id, |env| {
                Storage::get_hunt(env, template_hunt_id).unwrap()
            });
            template_hunt.reward_config =
                crate::types::HuntRewardConfig::new(&env, 0, false, None, 1, 0, 0, None);
            as_core_contract(&env, &contract_id, |env| {
                Storage::save_hunt(env, &template_hunt);
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::add_clue(
                    env.clone(),
                    template_hunt_id,
                    q1,
                    a1.clone(),
                    10,
                    true,
                    Some(1),
                    None,
                )
                .unwrap();
            });
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::add_clue(
                    env.clone(),
                    template_hunt_id,
                    q2,
                    a2.clone(),
                    20,
                    false,
                    Some(1),
                    None,
                )
                .unwrap();
            });
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::activate_hunt(env.clone(), template_hunt_id, template_creator.clone())
                    .unwrap();
            });
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::register_player(env.clone(), template_hunt_id, player.clone()).unwrap();
            });
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer(
                    env.clone(),
                    template_hunt_id,
                    1,
                    player.clone(),
                    a1.clone(),
                    4, /* nonce */
                    env.ledger().timestamp(),
                )
                .unwrap();
            });
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::complete_hunt(env.clone(), template_hunt_id, player.clone()).unwrap();
            });

            let template_hunt = as_core_contract(&env, &contract_id, |env| {
                Storage::get_hunt(env, template_hunt_id).unwrap()
            });
            let template_clues = as_core_contract(&env, &contract_id, |env| {
                Storage::list_clues_for_hunt(env, template_hunt_id, 0, 100)
            });

            let cloned_hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt_from_template(
                    env.clone(),
                    template_hunt_id,
                    new_creator.clone(),
                    cloned_title,
                    cloned_description,
                    None,
                    None,
                )
                .unwrap()
            });

            let cloned_hunt = as_core_contract(&env, &contract_id, |env| {
                Storage::get_hunt(env, cloned_hunt_id).unwrap()
            });
            let cloned_clues = as_core_contract(&env, &contract_id, |env| {
                Storage::list_clues_for_hunt(env, cloned_hunt_id, 0, 100)
            });

            assert_eq!(template_hunt.status, HuntStatus::Completed);
            assert_eq!(cloned_hunt.status, HuntStatus::Draft);
            assert_eq!(cloned_hunt.creator, new_creator);
            assert_eq!(cloned_hunt.total_clues, 2);
            assert_eq!(cloned_hunt.required_clues, 1);
            assert_eq!(template_clues.len(), cloned_clues.len());

            for i in 0..template_clues.len() {
                assert_eq!(template_clues.get(i).unwrap(), cloned_clues.get(i).unwrap());
            }
        }

        #[test]
        fn test_create_hunt_from_template_rejects_incomplete_template() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let contract_id = env.register_contract(None, super::HuntyCore);

            let creator = Address::generate(&env);
            let new_creator = Address::generate(&env);
            let title = String::from_str(&env, "Template Hunt");
            let description = String::from_str(&env, "Not completed yet");
            let q = String::from_str(&env, "Question?");
            let a = String::from_str(&env, "answer");

            let template_hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap()
            });
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::add_clue(env.clone(), template_hunt_id, q, a, 10, true, Some(1), None)
                    .unwrap();
            });
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::activate_hunt(env.clone(), template_hunt_id, creator.clone()).unwrap();
            });

            let err = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt_from_template(
                    env.clone(),
                    template_hunt_id,
                    new_creator,
                    String::from_str(env, "Cloned"),
                    String::from_str(env, "Draft from template"),
                    None,
                    None,
                )
                .unwrap_err()
            });

            assert_eq!(err, HuntErrorCode::InvalidHuntStatus);
        }

        // ========== clone_hunt() Tests ==========

        #[test]
        fn test_submit_answer_with_hash_enforces_cooldown() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator = Address::generate(&env);
            let player = Address::generate(&env);
            let contract_id = env.register_contract(None, super::HuntyCore);
            let question = String::from_str(&env, "What is 2 + 2?");
            let answer = String::from_str(&env, "four");

            env.mock_all_auths();
            let hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    String::from_str(env, "Cooldown Hunt"),
                    String::from_str(env, "Hash cooldown"),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap()
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::set_max_attempts_per_clue(
                    env.clone(),
                    hunt_id,
                    creator.clone(),
                    3,
                    30,
                )
                .unwrap();
                HuntyCore::add_clue(
                    env.clone(),
                    hunt_id,
                    question,
                    answer.clone(),
                    10,
                    true,
                    Some(1),
                    None,
                )
                .unwrap();
                HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
                HuntyCore::register_player(env.clone(), hunt_id, player.clone()).unwrap();
            });

            let hash = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::normalize_and_hash_answer(&env, hunt_id, 1, &answer).unwrap()
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer(
                    env.clone(),
                    hunt_id,
                    1,
                    player.clone(),
                    answer.clone(),
                    1,
                    env.ledger().timestamp(),
                )
                .unwrap();
            });

            env.mock_all_auths();
            let err = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer_with_hash(
                    env.clone(),
                    hunt_id,
                    1,
                    player.clone(),
                    hash,
                    2,
                    env.ledger().timestamp(),
                )
                .unwrap_err()
            });
            assert_eq!(err, HuntErrorCode::from(HuntError::AttemptCooldownNotExpired));

            env.ledger().set_timestamp(1_700_000_000 + 31);
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer_with_hash(
                    env.clone(),
                    hunt_id,
                    1,
                    player.clone(),
                    hash,
                    3,
                    env.ledger().timestamp(),
                )
                .unwrap();
            });
        }

        #[test]
        fn test_clone_hunt_rehashes_answers_and_preserves_creator_settings() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let creator = Address::generate(&env);
            let player = Address::generate(&env);
            let contract_id = env.register_contract(None, super::HuntyCore);

            env.mock_all_auths();
            let original_hunt_id = as_core_contract(&env, &contract_id, |env| {
                let hunt_id = HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    String::from_str(env, "Original"),
                    String::from_str(env, "Original"),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                HuntyCore::set_hunt_categories(
                    env.clone(),
                    hunt_id,
                    creator.clone(),
                    Vec::from_array(&env, [String::from_str(env, "alpha"), String::from_str(env, "beta")]),
                )
                .unwrap();
                HuntyCore::set_hunt_difficulty_override(
                    env.clone(),
                    hunt_id,
                    creator.clone(),
                    Some(4),
                )
                .unwrap();
                HuntyCore::set_time_bonus_config(
                    env.clone(),
                    hunt_id,
                    creator.clone(),
                    Some(TimeBonusConfig {
                        start_multiplier_bps: 30_000,
                        min_multiplier_bps: 12_000,
                        decay_duration_secs: 75,
                    }),
                )
                .unwrap();
                HuntyCore::set_max_attempts_per_clue(
                    env.clone(),
                    hunt_id,
                    creator.clone(),
                    4,
                    15,
                )
                .unwrap();
                HuntyCore::set_max_players(env.clone(), hunt_id, creator.clone(), 7).unwrap();
                HuntyCore::set_hunt_privacy(env.clone(), hunt_id, creator.clone(), true).unwrap();
                HuntyCore::generate_invite_code(
                    env.clone(),
                    hunt_id,
                    creator.clone(),
                    String::from_str(env, "original-secret"),
                )
                .unwrap();
                HuntyCore::add_clue(
                    env.clone(),
                    hunt_id,
                    String::from_str(env, "What is 2 + 2?"),
                    String::from_str(env, "four"),
                    10,
                    true,
                    Some(2),
                    None,
                )
                .unwrap();
                HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
                HuntyCore::register_player(env.clone(), hunt_id, player.clone()).unwrap();
                HuntyCore::submit_answer(
                    env.clone(),
                    hunt_id,
                    1,
                    player.clone(),
                    String::from_str(env, "four"),
                    1,
                    env.ledger().timestamp(),
                )
                .unwrap();
                HuntyCore::complete_hunt(env.clone(), hunt_id, player.clone()).unwrap();
                hunt_id
            });

            let original_hunt = as_core_contract(&env, &contract_id, |env| {
                Storage::get_hunt(env, original_hunt_id).unwrap()
            });
            let original_clue = as_core_contract(&env, &contract_id, |env| {
                Storage::get_clue(env, original_hunt_id, 1).unwrap()
            });

            env.mock_all_auths();
            let cloned_hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::clone_hunt(env.clone(), original_hunt_id, creator.clone()).unwrap()
            });

            let cloned_hunt = as_core_contract(&env, &contract_id, |env| {
                Storage::get_hunt(env, cloned_hunt_id).unwrap()
            });
            let cloned_clue = as_core_contract(&env, &contract_id, |env| {
                Storage::get_clue(env, cloned_hunt_id, 1).unwrap()
            });

            assert_eq!(cloned_hunt.categories.len(), original_hunt.categories.len());
            assert_eq!(cloned_hunt.time_bonus_start_bps, Some(30_000));
            assert_eq!(cloned_hunt.time_bonus_min_bps, Some(12_000));
            assert_eq!(cloned_hunt.time_bonus_decay_secs, Some(75));
            assert_eq!(cloned_hunt.max_attempts_per_clue, 4);
            assert_eq!(cloned_hunt.attempt_cooldown_secs, 15);
            assert_eq!(cloned_hunt.max_players, 7);
            assert_eq!(cloned_hunt.is_private, true);
            assert_eq!(cloned_hunt.difficulty_override, Some(4));
            assert_eq!(cloned_hunt.invite_code_hash, None);
            assert_ne!(original_clue.answer_hashes.get(0).unwrap(), cloned_clue.answer_hashes.get(0).unwrap());

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::generate_invite_code(
                    env.clone(),
                    cloned_hunt_id,
                    creator.clone(),
                    String::from_str(env, "clone-secret"),
                )
                .unwrap();
                HuntyCore::register_with_invite(
                    env.clone(),
                    cloned_hunt_id,
                    player.clone(),
                    String::from_str(env, "clone-secret"),
                )
                .unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer(
                    env.clone(),
                    cloned_hunt_id,
                    1,
                    player.clone(),
                    String::from_str(env, "four"),
                    2,
                    env.ledger().timestamp(),
                )
                .unwrap();
            });
        }

        #[test]
        fn test_calculate_score_uses_configured_time_bonus() {
            let env = Env::default();
            let hunt = crate::types::Hunt {
                hunt_id: 1,
                creator: Address::generate(&env),
                title: String::from_str(&env, "Configured"),
                description: String::from_str(&env, "Configured"),
                categories: Vec::new(&env),
                difficulty_rating: 0,
                difficulty_override: None,
                status: HuntStatus::Active,
                created_at: 0,
                activated_at: 0,
                start_time: 0,
                end_time: 0,
                reward_config: crate::types::HuntRewardConfig::new(&env, 0, false, None, 0, 0, 0, None),
                time_bonus_start_bps: Some(30_000),
                time_bonus_min_bps: Some(12_000),
                time_bonus_decay_secs: Some(60),
                total_clues: 0,
                required_clues: 0,
                completed_count: 0,
                max_submissions_per_minute: 0,
                max_attempts_per_clue: 5,
                start_multiplier_bps: 20_000,
                registration_deadline: 0,
                allow_partial_scoring: false,
                team_mode: false,
                default_points: 0,
                attempt_cooldown_secs: 0,
                max_players: 0,
                is_private: false,
                invite_code_hash: None,
                remaining_slots: 0,
                leaderboard_visibility: crate::types::LeaderboardVisibility::Public,
            };
            let clue = crate::types::Clue {
                clue_id: 1,
                question: String::from_str(&env, "Q"),
                answer_hashes: Vec::new(&env),
                points: 100,
                is_required: true,
                difficulty: 2,
                weight: 1,
                hint: None,
                hint_penalty_points: 0,
            };

            let at_start = HuntyCore::calculate_score(&hunt, &clue, 0, 0);
            let at_half = HuntyCore::calculate_score(&hunt, &clue, 0, 30);
            let at_min = HuntyCore::calculate_score(&hunt, &clue, 0, 120);

            assert_eq!(at_start, 600);
            assert_eq!(at_half, 420);
            assert_eq!(at_min, 240);
        }

        #[test]
        fn test_clone_hunt_creates_draft_with_clues() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let contract_id = env.register_contract(None, super::HuntyCore);

            let creator = Address::generate(&env);
            let player = Address::generate(&env);
            let title = String::from_str(&env, "Original Hunt");
            let description = String::from_str(&env, "Original description");
            let q1 = String::from_str(&env, "What is 2 + 2?");
            let q2 = String::from_str(&env, "What is 3 + 3?");
            let a1 = String::from_str(&env, "four");
            let a2 = String::from_str(&env, "six");

            // Create and complete original hunt
            env.mock_all_auths();
            let original_hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title.clone(),
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap()
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::add_clue(
                    env.clone(),
                    original_hunt_id,
                    q1.clone(),
                    a1.clone(),
                    10,
                    true,
                    Some(2),
                    None,
                )
                .unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::add_clue(
                    env.clone(),
                    original_hunt_id,
                    q2.clone(),
                    a2.clone(),
                    20,
                    false,
                    Some(3),
                    None,
                )
                .unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::activate_hunt(env.clone(), original_hunt_id, creator.clone()).unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::register_player(env.clone(), original_hunt_id, player.clone()).unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer(
                    env.clone(),
                    original_hunt_id,
                    1,
                    player.clone(),
                    a1.clone(),
                    1,
                    env.ledger().timestamp(),
                )
                .unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::complete_hunt(env.clone(), original_hunt_id, player.clone()).unwrap();
            });

            // Clone the hunt
            env.mock_all_auths();
            let cloned_hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::clone_hunt(env.clone(), original_hunt_id, creator.clone()).unwrap()
            });

            // Verify cloned hunt
            let original_hunt = as_core_contract(&env, &contract_id, |env| {
                Storage::get_hunt(env, original_hunt_id).unwrap()
            });

            let cloned_hunt = as_core_contract(&env, &contract_id, |env| {
                Storage::get_hunt(env, cloned_hunt_id).unwrap()
            });

            let original_clues = as_core_contract(&env, &contract_id, |env| {
                Storage::list_clues_for_hunt(env, original_hunt_id, 0, 100)
            });

            let cloned_clues = as_core_contract(&env, &contract_id, |env| {
                Storage::list_clues_for_hunt(env, cloned_hunt_id, 0, 100)
            });

            // Assert: new hunt ID
            assert_ne!(cloned_hunt_id, original_hunt_id);

            // Assert: cloned hunt is Draft
            assert_eq!(original_hunt.status, HuntStatus::Completed);
            assert_eq!(cloned_hunt.status, HuntStatus::Draft);

            // Assert: same creator
            assert_eq!(cloned_hunt.creator, creator);

            // Assert: clue configuration copied
            assert_eq!(cloned_hunt.total_clues, 2);
            assert_eq!(cloned_hunt.required_clues, 1);
            assert_eq!(cloned_clues.len(), 2);
            assert_eq!(original_clues.len(), cloned_clues.len());

            // Assert: clue content matches but IDs differ
            for i in 0..original_clues.len() {
                let orig = original_clues.get(i).unwrap();
                let cloned = cloned_clues.get(i).unwrap();
                assert_eq!(orig.question, cloned.question);
                assert_eq!(orig.points, cloned.points);
                assert_eq!(orig.is_required, cloned.is_required);
                assert_eq!(orig.difficulty, cloned.difficulty);
                // New clue IDs generated
                assert_ne!(orig.clue_id, cloned.clue_id);
            }

            // Verify HuntClonedEvent emitted
            let (topics, event) = find_event::<crate::types::HuntClonedEvent>(&env, "HuntCloned")
                .expect("HuntClonedEvent should be emitted");
            assert_eq!(event.original_hunt_id, original_hunt_id);
            assert_eq!(event.new_hunt_id, cloned_hunt_id);
            assert_eq!(event.creator, creator);
        }

        #[test]
        fn test_clone_hunt_does_not_copy_player_data() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let contract_id = env.register_contract(None, super::HuntyCore);

            let creator = Address::generate(&env);
            let player1 = Address::generate(&env);
            let player2 = Address::generate(&env);
            let title = String::from_str(&env, "Original Hunt");
            let description = String::from_str(&env, "With players");
            let q = String::from_str(&env, "Question?");
            let a = String::from_str(&env, "answer");

            // Create completed hunt with multiple players
            env.mock_all_auths();
            let original_hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap()
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::add_clue(env.clone(), original_hunt_id, q, a.clone(), 10, true, None, None)
                    .unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::activate_hunt(env.clone(), original_hunt_id, creator.clone()).unwrap();
            });

            // Register multiple players
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::register_player(env.clone(), original_hunt_id, player1.clone())
                    .unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::register_player(env.clone(), original_hunt_id, player2.clone())
                    .unwrap();
            });

            // Player1 completes
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer(
                    env.clone(),
                    original_hunt_id,
                    1,
                    player1.clone(),
                    a.clone(),
                    1,
                    env.ledger().timestamp(),
                )
                .unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::complete_hunt(env.clone(), original_hunt_id, player1.clone()).unwrap();
            });

            // Verify original hunt has players
            let original_player_count = as_core_contract(&env, &contract_id, |env| {
                Storage::get_player_count(env, original_hunt_id)
            });
            assert_eq!(original_player_count, 2);

            let original_player1_progress = as_core_contract(&env, &contract_id, |env| {
                Storage::get_player_progress(env, original_hunt_id, &player1)
            });
            assert!(original_player1_progress.is_some());
            assert!(original_player1_progress.unwrap().is_completed);

            // Clone the hunt
            env.mock_all_auths();
            let cloned_hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::clone_hunt(env.clone(), original_hunt_id, creator.clone()).unwrap()
            });

            // Assert: cloned hunt has NO player data
            let cloned_player_count = as_core_contract(&env, &contract_id, |env| {
                Storage::get_player_count(env, cloned_hunt_id)
            });
            assert_eq!(cloned_player_count, 0);

            // Assert: no player progress exists for cloned hunt
            let cloned_player1_progress = as_core_contract(&env, &contract_id, |env| {
                Storage::get_player_progress(env, cloned_hunt_id, &player1)
            });
            assert!(cloned_player1_progress.is_none());

            let cloned_player2_progress = as_core_contract(&env, &contract_id, |env| {
                Storage::get_player_progress(env, cloned_hunt_id, &player2)
            });
            assert!(cloned_player2_progress.is_none());

            // Assert: cloned hunt completed_count is 0
            let cloned_hunt = as_core_contract(&env, &contract_id, |env| {
                Storage::get_hunt(env, cloned_hunt_id).unwrap()
            });
            assert_eq!(cloned_hunt.completed_count, 0);
        }

        #[test]
        fn test_clone_hunt_requires_creator_authorization() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let contract_id = env.register_contract(None, super::HuntyCore);

            let original_creator = Address::generate(&env);
            let unauthorized_user = Address::generate(&env);
            let player = Address::generate(&env);
            let title = String::from_str(&env, "Original Hunt");
            let description = String::from_str(&env, "Creator only");
            let q = String::from_str(&env, "Question?");
            let a = String::from_str(&env, "answer");

            // Create and complete hunt as original_creator
            env.mock_all_auths();
            let hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt(
                    env.clone(),
                    original_creator.clone(),
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap()
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::add_clue(env.clone(), hunt_id, q, a.clone(), 10, true, None, None)
                    .unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::activate_hunt(env.clone(), hunt_id, original_creator.clone()).unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::register_player(env.clone(), hunt_id, player.clone()).unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::submit_answer(
                    env.clone(),
                    hunt_id,
                    1,
                    player.clone(),
                    a,
                    1,
                    env.ledger().timestamp(),
                )
                .unwrap();
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::complete_hunt(env.clone(), hunt_id, player.clone()).unwrap();
            });

            // Attempt to clone as unauthorized user
            env.mock_all_auths();
            let err = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::clone_hunt(env.clone(), hunt_id, unauthorized_user.clone()).unwrap_err()
            });

            assert_eq!(err, HuntErrorCode::Unauthorized);

            // Verify original creator CAN clone
            env.mock_all_auths();
            let cloned_hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::clone_hunt(env.clone(), hunt_id, original_creator.clone()).unwrap()
            });

            assert_ne!(cloned_hunt_id, hunt_id);
        }

        #[test]
        fn test_clone_hunt_requires_completed_status() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let contract_id = env.register_contract(None, super::HuntyCore);

            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Must be completed first");
            let q = String::from_str(&env, "Question?");
            let a = String::from_str(&env, "answer");

            // Create hunt in Draft status
            env.mock_all_auths();
            let hunt_id = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap()
            });

            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::add_clue(env.clone(), hunt_id, q, a, 10, true, None, None).unwrap();
            });

            // Attempt to clone Draft hunt
            env.mock_all_auths();
            let err_draft = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::clone_hunt(env.clone(), hunt_id, creator.clone()).unwrap_err()
            });

            assert_eq!(err_draft, HuntErrorCode::InvalidHuntStatus);

            // Activate hunt
            env.mock_all_auths();
            as_core_contract(&env, &contract_id, |env| {
                HuntyCore::activate_hunt(env.clone(), hunt_id, creator.clone()).unwrap();
            });

            // Attempt to clone Active hunt
            env.mock_all_auths();
            let err_active = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::clone_hunt(env.clone(), hunt_id, creator.clone()).unwrap_err()
            });

            assert_eq!(err_active, HuntErrorCode::InvalidHuntStatus);
        }

        #[test]
        fn test_clone_hunt_nonexistent_hunt() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            let contract_id = env.register_contract(None, super::HuntyCore);

            let creator = Address::generate(&env);

            // Attempt to clone non-existent hunt
            env.mock_all_auths();
            let err = as_core_contract(&env, &contract_id, |env| {
                HuntyCore::clone_hunt(env.clone(), 9999, creator.clone()).unwrap_err()
            });

            assert_eq!(err, HuntErrorCode::HuntNotFound);
        }

        // ========== add_clue() / get_clue() / list_clues() Tests ==========

        #[test]
        fn test_add_clue_success() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Test Hunt");
            let description = String::from_str(&env, "Description");
            let question = String::from_str(&env, "What is 2 + 2?");
            let answer = String::from_str(&env, "four");

            let (hunt_id, clue_id, hunt, info) = with_core_contract(&env, |env, _cid| {
                let hunt_id = HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title,
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let clue_id = HuntyCore::add_clue(
                    env.clone(),
                    hunt_id,
                    question.clone(),
                    answer,
                    10,
                    true,
                    Some(1),
                    None,
                )
                .unwrap();
                let hunt = Storage::get_hunt(env, hunt_id).unwrap();
                let info = HuntyCore::get_clue(env.clone(), hunt_id, clue_id).unwrap();
                (hunt_id, clue_id, hunt, info)
            });

            assert_eq!(hunt_id, 1);
            assert_eq!(clue_id, 1);
            assert_eq!(hunt.total_clues, 1);
            assert_eq!(info.clue_id, 1);
            assert_eq!(info.question, question);
            assert_eq!(info.points, 10);
            assert!(info.is_required);
        }

        #[test]
        #[should_panic]
        fn test_add_clue_unauthorized() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            // Do NOT mock auth â€” require_auth(creator) will fail.
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Test Hunt");
            let description = String::from_str(&env, "Description");
            let question = String::from_str(&env, "What is 2 + 2?");
            let answer = String::from_str(&env, "four");

            with_core_contract(&env, |env, _cid| {
                let hunt_id = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let _ = HuntyCore::add_clue(
                    env.clone(),
                    hunt_id,
                    question,
                    answer,
                    10,
                    true,
                    Some(1),
                    None,
                )
                .unwrap();
            });
        }

        #[test]
        fn test_add_clue_sequential_ids() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Desc");
            let q1 = String::from_str(&env, "Q1");
            let q2 = String::from_str(&env, "Q2");
            let q3 = String::from_str(&env, "Q3");
            let a = String::from_str(&env, "a");

            let (id1, id2, id3) = with_core_contract(&env, |env, _cid| {
                let hid = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let id1 =
                    HuntyCore::add_clue(env.clone(), hid, q1, a.clone(), 1, false, Some(1), None)
                        .unwrap();
                let id2 =
                    HuntyCore::add_clue(env.clone(), hid, q2, a.clone(), 1, false, Some(1), None)
                        .unwrap();
                let id3 =
                    HuntyCore::add_clue(env.clone(), hid, q3, a, 1, false, Some(1), None).unwrap();
                (id1, id2, id3)
            });

            assert_eq!(id1, 1);
            assert_eq!(id2, 2);
            assert_eq!(id3, 3);
        }

        #[test]
        fn test_add_clue_answer_normalization_and_hashing() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Desc");
            let question = String::from_str(&env, "Same answer?");
            let answer1 = String::from_str(&env, "  ANSWER  ");
            let answer2 = String::from_str(&env, "answer");

            let (hash1, hash2) = with_core_contract(&env, |env, _cid| {
                let hid = HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title.clone(),
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let cid = HuntyCore::add_clue(
                    env.clone(),
                    hid,
                    question.clone(),
                    answer1,
                    5,
                    true,
                    Some(1),
                    None,
                )
                .unwrap();
                let c = Storage::get_clue(env, hid, cid).unwrap();
                let h1 = c.answer_hashes.get(0).unwrap();
                let hid2 = HuntyCore::create_hunt(
                    env.clone(),
                    Address::generate(&env),
                    String::from_str(&env, "H2"),
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let _cid2 = HuntyCore::add_clue(
                    env.clone(),
                    hid2,
                    question,
                    answer2,
                    5,
                    false,
                    Some(1),
                    None,
                )
                .unwrap();
                let c2 = Storage::get_clue(env, hid2, _cid2).unwrap();
                let h2 = c2.answer_hashes.get(0).unwrap();
                (h1, h2)
            });

            assert_eq!(
                hash1, hash2,
                "normalized '  ANSWER  ' and 'answer' must hash the same"
            );
        }

        #[test]
        fn test_add_clue_whitespace_answer_normalization_and_hashing() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Desc");
            let question = String::from_str(&env, "Whitespace answer?");
            let answer1 = String::from_str(&env, "\t\n answer \r\n");
            let answer2 = String::from_str(&env, "answer");

            let (hash1, hash2) = with_core_contract(&env, |env, _cid| {
                let hid = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let cid = HuntyCore::add_clue(
                    env.clone(),
                    hid,
                    question.clone(),
                    answer1,
                    5,
                    false,
                    Some(1),
                    None,
                )
                .unwrap();
                let c = Storage::get_clue(env, hid, cid).unwrap();
                let h1 = c.answer_hashes.get(0).unwrap();
                let hid2 = HuntyCore::create_hunt(
                    env.clone(),
                    Address::generate(&env),
                    String::from_str(&env, "H2"),
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let _cid2 = HuntyCore::add_clue(
                    env.clone(),
                    hid2,
                    question,
                    answer2,
                    5,
                    false,
                    Some(1),
                    None,
                )
                .unwrap();
                let c2 = Storage::get_clue(env, hid2, _cid2).unwrap();
                let h2 = c2.answer_hashes.get(0).unwrap();
                (h1, h2)
            });

            assert_eq!(
                hash1, hash2,
                "normalized '\t\n answer \r\n' and 'answer' must hash the same"
            );
        }

        #[test]
        fn test_add_clue_unicode_answer_normalization_and_hashing() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Desc");
            let question = String::from_str(&env, "Same answer?");
            let answer1 = String::from_str(&env, "CafÃ©");
            let answer2 = String::from_str(&env, "cafÃ©");

            let (hash1, hash2) = with_core_contract(&env, |env, _cid| {
                let hid = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let cid = HuntyCore::add_clue(
                    env.clone(),
                    hid,
                    question.clone(),
                    answer1,
                    5,
                    false,
                    Some(1),
                    None,
                )
                .unwrap();
                let c = Storage::get_clue(env, hid, cid).unwrap();
                let h1 = c.answer_hashes.get(0).unwrap();
                let hid2 = HuntyCore::create_hunt(
                    env.clone(),
                    Address::generate(&env),
                    String::from_str(&env, "H2"),
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let _cid2 = HuntyCore::add_clue(
                    env.clone(),
                    hid2,
                    question,
                    answer2,
                    5,
                    false,
                    Some(1),
                    None,
                )
                .unwrap();
                let c2 = Storage::get_clue(env, hid2, _cid2).unwrap();
                let h2 = c2.answer_hashes.get(0).unwrap();
                (h1, h2)
            });

            assert_eq!(
                hash1, hash2,
                "normalized 'CafÃ©' and 'cafÃ©' must hash the same"
            );
        }

        #[test]
        fn test_get_clue_excludes_answer_hash() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Desc");
            let question = String::from_str(&env, "Secret?");
            let answer = String::from_str(&env, "secret");

            let info = with_core_contract(&env, |env, _cid| {
                let hid = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                let _ = HuntyCore::add_clue(
                    env.clone(),
                    hid,
                    question.clone(),
                    answer,
                    7,
                    true,
                    Some(1),
                    None,
                )
                .unwrap();
                HuntyCore::get_clue(env.clone(), hid, 1).unwrap()
            });

            // Prove at compile-time that `ClueInfo` has exactly these fields, and NO `answer_hash` field.
            // The raw `Clue` (with hash) cannot be fetched through the public API (`get_clue` returns `ClueInfo`).
            let ClueInfo {
                clue_id,
                question: ret_question,
                points,
                is_required,
                ..
            } = info;

            assert_eq!(clue_id, 1);
            assert_eq!(ret_question, question);
            assert_eq!(points, 7);
            assert!(is_required);
        }

        #[test]
        fn test_get_clue_not_found() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Desc");

            let err = with_core_contract(&env, |env, _cid| {
                let hid = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                HuntyCore::get_clue(env.clone(), hid, 999).unwrap_err()
            });

            assert_eq!(err, HuntErrorCode::ClueNotFound);
        }

        #[test]
        fn test_list_clues_empty() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Desc");

            let list = with_core_contract(&env, |env, _cid| {
                let hid = HuntyCore::create_hunt(
                    env.clone(),
                    creator.clone(),
                    title.clone(),
                    description.clone(),
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                HuntyCore::list_clues(env.clone(), hid, 0, 10)
            });

            let expected = Vec::new(&env);
            assert_eq!(list, expected);
        }

        #[test]
        fn test_list_clues_returns_all() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Desc");
            let q1 = String::from_str(&env, "Q1");
            let q2 = String::from_str(&env, "Q2");
            let a = String::from_str(&env, "a");

            let list = with_core_contract(&env, |env, _cid| {
                let hid = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                HuntyCore::add_clue(env.clone(), hid, q1, a.clone(), 1, false, Some(1), None)
                    .unwrap();
                HuntyCore::add_clue(env.clone(), hid, q2, a, 2, true, Some(1), None).unwrap();
                HuntyCore::list_clues(env.clone(), hid, 0, 10)
            });

            assert_eq!(list.len(), 2);
            let c1 = list.get(0).unwrap();
            let c2 = list.get(1).unwrap();
            assert_eq!(c1.clue_id, 1);
            assert_eq!(c2.clue_id, 2);
            assert_eq!(c1.points, 1);
            assert_eq!(c2.points, 2);
            assert!(!c1.is_required);
            assert!(c2.is_required);
        }

        #[test]
        fn test_list_clues_pagination() {
            let env = Env::default();
            env.ledger().set_timestamp(1_700_000_000);
            env.mock_all_auths();
            let creator = Address::generate(&env);
            let title = String::from_str(&env, "Hunt");
            let description = String::from_str(&env, "Desc");
            let q1 = String::from_str(&env, "Q1");
            let q2 = String::from_str(&env, "Q2");
            let q3 = String::from_str(&env, "Q3");
            let a = String::from_str(&env, "a");

            let (list1, list2, list_all) = with_core_contract(&env, |env, _cid| {
                let hid = HuntyCore::create_hunt(
                    env.clone(),
                    creator,
                    title,
                    description,
                    None,
                    None,
                    0,
                    None,
                    None,
                )
                .unwrap();
                HuntyCore::add_clue(env.clone(), hid, q1, a.clone(), 1, false, Some(1), None)
           