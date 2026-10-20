//! Regression tests for #1249: cloning must support primary answers and aliases.

use crate::errors::HuntErrorCode;
use crate::storage::Storage;
use crate::types::HuntStatus;
use crate::{HuntyCore, HuntyCoreClient, MAX_ALIASES_PER_CLUE};
use soroban_sdk::testutils::{Address as _, Ledger as _, MockAuth, MockAuthInvoke};
use soroban_sdk::{vec, Address, Env, IntoVal, String, Val, Vec};

const T0: u64 = 1_700_000_000;

fn answers(env: &Env, groups: &[&[&str]]) -> Vec<Vec<String>> {
    let mut result = Vec::new(env);
    for group in groups {
        let mut clue_answers = Vec::new(env);
        for answer in *group {
            clue_answers.push_back(String::from_str(env, answer));
        }
        result.push_back(clue_answers);
    }
    result
}

fn with_creator_auth<T>(
    env: &Env,
    contract_id: &Address,
    creator: &Address,
    fn_name: &str,
    args: Vec<Val>,
    call: impl FnOnce(&HuntyCoreClient) -> T,
) -> T {
    let invocation = MockAuthInvoke {
        contract: contract_id,
        fn_name,
        args,
        sub_invokes: &[],
    };
    let auth = MockAuth {
        address: creator,
        invoke: &invocation,
    };
    // Cloning and its nested create_hunt both require creator auth in the same
    // frame. Explicit entries avoid mock_all_auths recording that frame twice.
    call(&HuntyCoreClient::new(env, contract_id).mock_auths(&[auth.clone(), auth]))
}

fn try_clone(
    env: &Env,
    contract_id: &Address,
    template_id: u64,
    creator: &Address,
    supplied: &Vec<Vec<String>>,
) -> Result<u64, HuntErrorCode> {
    let result = with_creator_auth(
        env,
        contract_id,
        creator,
        "clone_hunt_with_answers",
        (template_id, creator.clone(), supplied.clone()).into_val(env),
        |client| client.try_clone_hunt_with_answers(&template_id, creator, supplied),
    );
    match result {
        Ok(Ok(hunt_id)) => Ok(hunt_id),
        Err(Ok(error)) => Err(error),
        other => panic!("unexpected clone invocation result: {other:?}"),
    }
}

fn completed_template() -> (Env, Address, Address, u64) {
    let env = Env::default();
    env.ledger().set_timestamp(T0);
    env.mock_all_auths();
    let creator = Address::generate(&env);
    let contract_id = env.register(HuntyCore, (&creator,));
    let client = HuntyCoreClient::new(&env, &contract_id);
    let hunt_id = client.create_hunt(
        &creator,
        &String::from_str(&env, "Template"),
        &String::from_str(&env, "Clone with aliases"),
        &None,
        &None,
        &0,
        &None,
        &None,
    );
    for (primary, aliases) in [
        ("New York", ["Big Apple", "NYC"]),
        ("Paris", ["City of Light", "Lutetia"]),
    ] {
        let clue_id = client.add_clue(
            &hunt_id,
            &String::from_str(&env, "Which city?"),
            &String::from_str(&env, primary),
            &10,
            &true,
            &None,
            &None,
        );
        client.add_clue_aliases(
            &hunt_id,
            &clue_id,
            &vec![
                &env,
                String::from_str(&env, aliases[0]),
                String::from_str(&env, aliases[1]),
            ],
        );
    }
    client.activate_hunt(&hunt_id, &creator);
    client.close_hunt(&hunt_id, &creator);
    (env, contract_id, creator, hunt_id)
}

#[test]
fn clone_rehashes_primary_answers_and_all_supplied_aliases() {
    let (env, contract_id, creator, template_id) = completed_template();
    let client = HuntyCoreClient::new(&env, &contract_id);
    let supplied = answers(
        &env,
        &[
            &["New York", "Big Apple", "NYC"],
            &["Paris", "City of Light", "Lutetia"],
        ],
    );
    let clone_id = try_clone(&env, &contract_id, template_id, &creator, &supplied).unwrap();

    env.as_contract(&contract_id, || {
        assert_eq!(
            Storage::get_hunt(&env, clone_id).unwrap().status,
            HuntStatus::Draft
        );
        for clue_id in 1..=2 {
            let template = Storage::get_clue(&env, template_id, clue_id).unwrap();
            let cloned = Storage::get_clue(&env, clone_id, clue_id).unwrap();
            assert_eq!(cloned.answer_hashes.len(), 3);
            assert_eq!(template.answer_hashes.len(), 3);
            for (index, answer) in supplied.get(clue_id - 1).unwrap().iter().enumerate() {
                let hash =
                    HuntyCore::normalize_and_hash_answer(&env, clone_id, clue_id, &answer).unwrap();
                assert_eq!(cloned.answer_hashes.get(index as u32).unwrap(), hash);
                assert!(!template.answer_hashes.contains(&hash));
            }
        }
    });

    client.activate_hunt(&clone_id, &creator);
    // Different players exercise the primary answer and both aliases for each clue.
    for [first, second] in [
        ["  NEW   YORK  ", "PARIS"],
        ["big apple", "city of light"],
        ["nyc", "lutetia"],
    ] {
        let player = Address::generate(&env);
        client.register_player(&clone_id, &player);
        assert!(!client.preview_answer(&clone_id, &1, &player, &String::from_str(&env, second)));
        for (clue_id, answer) in [(1, first), (2, second)] {
            assert!(client.submit_answer(
                &clone_id,
                &clue_id,
                &player,
                &String::from_str(&env, answer),
                &(clue_id as u64),
                &T0,
            ));
        }
    }
}

#[test]
fn clone_accepts_one_primary_answer_per_clue() {
    let (env, contract_id, creator, template_id) = completed_template();
    let supplied = answers(&env, &[&["New York"], &["Paris"]]);
    let clone_id = try_clone(&env, &contract_id, template_id, &creator, &supplied).unwrap();
    env.as_contract(&contract_id, || {
        for clue_id in 1..=2 {
            assert_eq!(
                Storage::get_clue(&env, clone_id, clue_id)
                    .unwrap()
                    .answer_hashes
                    .len(),
                1
            );
        }
    });
}

#[test]
fn clone_deduplicates_normalized_aliases_at_the_limit() {
    let (env, contract_id, creator, template_id) = completed_template();
    let mut clue_answers = answers(
        &env,
        &[&[
            "New York", "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
            "iota",
        ]],
    )
    .get(0)
    .unwrap();
    for _ in 0..=MAX_ALIASES_PER_CLUE {
        clue_answers.push_back(String::from_str(&env, " NEW   YORK "));
        clue_answers.push_back(String::from_str(&env, "new york"));
    }
    let clone_id = try_clone(
        &env,
        &contract_id,
        template_id,
        &creator,
        &vec![
            &env,
            clue_answers,
            vec![&env, String::from_str(&env, "Paris")],
        ],
    )
    .unwrap();
    env.as_contract(&contract_id, || {
        assert_eq!(
            Storage::get_clue(&env, clone_id, 1)
                .unwrap()
                .answer_hashes
                .len(),
            MAX_ALIASES_PER_CLUE
        );
    });
}

#[test]
fn clone_enforces_the_distinct_answer_limit_and_rolls_back() {
    let (env, contract_id, creator, template_id) = completed_template();
    let client = HuntyCoreClient::new(&env, &contract_id);
    let mut clue_answers = Vec::new(&env);
    for i in 0..=MAX_ALIASES_PER_CLUE {
        clue_answers.push_back(String::from_bytes(&env, &[b'a' + i as u8]));
    }
    let supplied = vec![
        &env,
        vec![&env, String::from_str(&env, "New York")],
        clue_answers.clone(),
    ];
    assert_eq!(
        try_clone(&env, &contract_id, template_id, &creator, &supplied),
        Err(HuntErrorCode::TooManyAliases)
    );
    assert_eq!(client.get_hunt_count(), 1);

    clue_answers.pop_back();
    let clone_id = try_clone(
        &env,
        &contract_id,
        template_id,
        &creator,
        &vec![
            &env,
            vec![&env, String::from_str(&env, "New York")],
            clue_answers,
        ],
    )
    .unwrap();
    env.as_contract(&contract_id, || {
        assert_eq!(
            Storage::get_clue(&env, clone_id, 2)
                .unwrap()
                .answer_hashes
                .len(),
            MAX_ALIASES_PER_CLUE
        );
    });
}

#[test]
fn clone_rejects_missing_or_extra_clue_answer_lists() {
    let (env, contract_id, creator, template_id) = completed_template();
    let client = HuntyCoreClient::new(&env, &contract_id);
    for supplied in [
        answers(&env, &[&["New York"]]),
        answers(&env, &[&["New York"], &["Paris"], &["Rome"]]),
    ] {
        assert_eq!(
            try_clone(&env, &contract_id, template_id, &creator, &supplied),
            Err(HuntErrorCode::InvalidAnswer)
        );
        assert_eq!(client.get_hunt_count(), 1);
    }
}

#[test]
fn clone_rejects_empty_lists_and_invalid_aliases_without_partial_writes() {
    let (env, contract_id, creator, template_id) = completed_template();
    let client = HuntyCoreClient::new(&env, &contract_id);
    let primary = String::from_str(&env, "Paris");
    for group in [
        Vec::new(&env),
        vec![&env, primary.clone(), String::from_str(&env, "")],
        vec![&env, primary.clone(), String::from_str(&env, " \t\n ")],
        vec![&env, primary, String::from_bytes(&env, &[b'a'; 257])],
    ] {
        let supplied = vec![&env, vec![&env, String::from_str(&env, "New York")], group];
        assert_eq!(
            try_clone(&env, &contract_id, template_id, &creator, &supplied),
            Err(HuntErrorCode::InvalidAnswer)
        );
        assert_eq!(client.get_hunt_count(), 1);
        env.as_contract(&contract_id, || {
            assert!(Storage::get_hunt(&env, template_id + 1).is_none());
            assert!(Storage::get_clue(&env, template_id + 1, 1).is_none());
        });
    }
}

#[test]
fn legacy_clone_paths_still_copy_template_hashes() {
    let (env, contract_id, creator, template_id) = completed_template();
    for clone_id in [
        with_creator_auth(
            &env,
            &contract_id,
            &creator,
            "clone_hunt",
            (template_id, creator.clone()).into_val(&env),
            |client| client.clone_hunt(&template_id, &creator),
        ),
        try_clone(&env, &contract_id, template_id, &creator, &Vec::new(&env)).unwrap(),
    ] {
        env.as_contract(&contract_id, || {
            for clue_id in 1..=2 {
                assert_eq!(
                    Storage::get_clue(&env, clone_id, clue_id)
                        .unwrap()
                        .answer_hashes,
                    Storage::get_clue(&env, template_id, clue_id)
                        .unwrap()
                        .answer_hashes
                );
            }
        });
    }
}
