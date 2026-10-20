//! Regression tests for #1254: title searches ignore ASCII letter case.

use crate::{HuntyCore, HuntyCoreClient, MAX_TITLE_BYTES};
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{vec, Address, Env, String, Vec};

fn fixture(titles: &[&str]) -> (Env, Address, Vec<u64>) {
    let env = Env::default();
    env.mock_all_auths();
    let creator = Address::generate(&env);
    let contract_id = env.register(HuntyCore, (&creator,));
    let client = HuntyCoreClient::new(&env, &contract_id);
    let mut ids = Vec::new(&env);
    for title in titles {
        ids.push_back(client.create_hunt(
            &creator,
            &String::from_str(&env, title),
            &String::from_str(&env, "Search fixture"),
            &None,
            &None,
            &0,
            &None,
            &None,
        ));
    }
    (env, contract_id, ids)
}

fn search_ids(env: &Env, contract_id: &Address, query: &str) -> Vec<u64> {
    let client = HuntyCoreClient::new(env, contract_id);
    let mut ids = Vec::new(env);
    for hunt in client.search_hunts(&String::from_str(env, query), &0, &10, &10) {
        ids.push_back(hunt.hunt_id);
    }
    ids
}

#[test]
fn search_hunts_matches_ascii_case_variants_and_partial_titles() {
    let (env, contract_id, ids) = fixture(&["Treasure Hunt", "Hidden TREASURE", "City Walk"]);
    let matching = vec![&env, ids.get(0).unwrap(), ids.get(1).unwrap()];
    for query in ["treasure", "TREASURE", "tReAsUrE", "eAsUr"] {
        assert_eq!(search_ids(&env, &contract_id, query), matching);
    }
    for query in ["tReAsUrE hUnT", "TrEaSuRe H", "hUnT"] {
        assert_eq!(
            search_ids(&env, &contract_id, query),
            vec![&env, ids.get(0).unwrap()]
        );
    }
    for query in ["treasures", "trea sure", "missing"] {
        assert!(search_ids(&env, &contract_id, query).is_empty());
    }
    let hunt = HuntyCoreClient::new(&env, &contract_id).get_hunt_info(&ids.get(0).unwrap());
    assert_eq!(hunt.title, String::from_str(&env, "Treasure Hunt"));
}

#[test]
fn search_hunts_preserves_non_ascii_bytes() {
    let (env, contract_id, ids) = fixture(&["École Café"]);
    for query in ["ÉCOLE", "é", "CAFé"] {
        assert_eq!(search_ids(&env, &contract_id, query), ids);
    }
    // Only ASCII letters fold: accents are neither lowercased nor removed.
    for query in ["école", "ÉCOLE CAFÉ", "cafe"] {
        assert!(search_ids(&env, &contract_id, query).is_empty());
    }
}

#[test]
fn search_hunts_preserves_empty_query_and_title_length_bounds() {
    let mut title = [b'X'; MAX_TITLE_BYTES as usize];
    title[MAX_TITLE_BYTES as usize - 8..].copy_from_slice(b"TrEaSuRe");
    let (env, contract_id, ids) = fixture(&[core::str::from_utf8(&title).unwrap()]);
    assert_eq!(search_ids(&env, &contract_id, ""), ids);
    assert_eq!(search_ids(&env, &contract_id, "treasure"), ids);
    title.make_ascii_lowercase();
    assert_eq!(
        search_ids(&env, &contract_id, core::str::from_utf8(&title).unwrap()),
        ids
    );
    let too_long = [b'X'; MAX_TITLE_BYTES as usize + 1];
    assert!(search_ids(&env, &contract_id, core::str::from_utf8(&too_long).unwrap()).is_empty());
}

#[test]
fn search_hunts_keeps_scan_and_page_limits() {
    let (env, contract_id, ids) = fixture(&["Treasure Hunt", "City Walk", "Hidden TREASURE"]);
    let client = HuntyCoreClient::new(&env, &contract_id);
    let query = String::from_str(&env, "tReAsUrE");
    let first = client.search_hunts(&query, &0, &1, &3);
    assert_eq!(first.len(), 1);
    assert_eq!(first.get(0).unwrap().hunt_id, ids.get(0).unwrap());
    assert!(client.search_hunts(&query, &1, &10, &1).is_empty());
    let next = client.search_hunts(&query, &1, &10, &2);
    assert_eq!(next.len(), 1);
    assert_eq!(next.get(0).unwrap().hunt_id, ids.get(2).unwrap());
}
