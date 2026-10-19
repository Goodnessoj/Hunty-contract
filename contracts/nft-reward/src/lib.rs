#![no_std]

mod migration;
mod storage;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, BytesN, Env, Map,
    String, Symbol, TryFromVal, Val, Vec,
};

pub const METADATA_SCHEMA_VERSION: u32 = 1;

/// Validate a user-supplied string through the sanitizer, mapping any
/// sanitization error to `NftErrorCode::InvalidMetadata`.
fn sanitize_string(
    env: &Env,
    input: &String,
    max_bytes: u32,
    allow_empty: bool,
) -> Result<String, NftErrorCode> {
    StringSanitizer::sanitize(env, input, max_bytes, allow_empty)
        .map_err(|_| NftErrorCode::InvalidMetadata)
}

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum NftErrorCode {
    NftNotFound = 1,
    Unauthorized = 2,
    NotOwner = 3,
    InvalidRecipient = 4,
    SoulboundNft = 5,
    InvalidRarity = 6,
    AlreadyInitialized = 7,
    MaxSupplyReached = 8,
    NotInitialized = 9,
    NotOperator = 10,
    NftNotTransferable = 11,
    NftLocked = 12,
    InvalidMetadata = 13,
    MetadataFrozen = 14,
    TooManyExtensions = 15,
    InvalidExtensionKey = 16,
    InvalidExtensionValue = 17,
    ExtensionNotFound = 18,
    InvalidMaxSupply = 19,
    InvalidRoyalty = 20,
    InvalidImageUri = 21,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectionMetadata {
    pub name: String,
    pub description: String,
    pub total_supply: u64,
    pub creator: Option<Address>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NftMetadata {
    pub title: String,
    pub description: String,
    pub image_uri: String,
    pub hunt_title: String,
    pub rarity: u32,
    pub tier: u32,
    pub creator: Option<Address>,
    pub royalty_bps: Option<u32>,
    pub extensions: Map<String, String>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Nft {
    pub nft_id: u64,
    pub hunt_id: u64,
    pub owner: Address,
    pub metadata: NftMetadata,
    pub minted_at: u64,
    pub transferable: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NftMintedEvent {
    pub nft_id: u64,
    pub hunt_id: u64,
    pub owner: Address,
    pub minted_at: u64,
    pub total_minted_for_hunt: u64,
}

#[contract]
pub struct NftReward;

#[contractimpl]
impl NftReward {
    /// Storage key for the whitelisted minter recorded at deployment time.
    const MINTER_KEY: Symbol = symbol_short!("MINTER");

    /// Constructor - runs atomically during deployment.
    /// Prevents front-running by initializing during deploy transaction.
    pub fn __constructor(
        env: Env,
        admin: Address,
        minter: Address,
        max_supply: Option<u64>,
        metadata: CollectionMetadata,
    ) {
        // Sanitize collection metadata strings
        sanitize_string(
            &env,
            &metadata.name,
            MAX_COLLECTION_NAME_BYTES,
            false,
        )
        .expect("invalid collection name");
        sanitize_string(
            &env,
            &metadata.description,
            MAX_COLLECTION_DESCRIPTION_BYTES,
            false,
        )
        .expect("invalid collection description");

        // Store admin
        env.storage()
            .instance()
            .set(&symbol_short!("ADMIN"), &admin);
        // Store minter
        env.storage()
            .instance()
            .set(&Self::MINTER_KEY, &minter);
        // Store max supply if provided
        if let Some(max) = max_supply {
            env.storage()
                .instance()
                .set(&symbol_short!("MAXSPLY"), &max);
        }
        // Store collection metadata
        env.storage()
            .instance()
            .set(&symbol_short!("COLMETA"), &metadata);
        // Initialize total supply to 0
        env.storage()
            .instance()
            .set(&symbol_short!("TOTSPLY"), &0u64);
        // Initialize NFT counter to 0
        env.storage()
            .instance()
            .set(&symbol_short!("NFTCNT"), &0u64);
        // Set upgrade admin to the constructor's admin so the first caller
        // of initialize_schema cannot hijack upgrade authority.
        hunty_migration::UpgradeAuthorization::set_upgrade_admin(&env, &admin);
    }

    pub fn set_reward_manager(
        env: Env,
        admin: Address,
        reward_manager: Address,
    ) -> Result<(), NftErrorCode> {
        admin.require_auth();
        let stored_admin = env
            .storage()
            .instance()
            .get::<_, Address>(&symbol_short!("ADMIN"))
            .ok_or(NftErrorCode::NotInitialized)?;
        if admin != stored_admin {
            return Err(NftErrorCode::Unauthorized);
        }

        env.storage()
            .instance()
            .set(&Self::MINTER_KEY, &reward_manager);
        env.events()
            .publish((symbol_short!("RWD_MGR"), admin), reward_manager);
        Ok(())
    }

    pub fn get_total_supply(_env: Env) -> u64 {
        0
    }

    pub fn get_max_supply(env: Env) -> Option<u64> {
        env.storage()
            .instance()
            .get(&symbol_short!("MAXSPLY"))
    }

    pub fn get_nft_metadata(_env: Env, _nft_id: u64) -> Option<NftMetadata> {
        None
    }

    /// Verifies that `minter` is allowed to mint reward NFTs.
    ///
    /// The caller must sign the call (`require_auth`) and must match the MINTER
    /// recorded by the constructor — anyone else is rejected with
    /// `NftErrorCode::Unauthorized`. If no MINTER is stored yet (pre-existing
    /// deployment that predates this guard) the whitelist check is skipped.
    fn require_minter(env: &Env, minter: &Address) -> Result<(), NftErrorCode> {
        minter.require_auth();
        match env.storage().instance().get::<_, Address>(&Self::MINTER_KEY) {
            Some(whitelisted) if &whitelisted == minter => Ok(()),
            Some(_) => Err(NftErrorCode::Unauthorized),
            None => Ok(()),
        }
    }

    pub fn mint_reward_nft(
        env: Env,
        minter: Address,
        _hunt_id: u64,
        _owner: Address,
        metadata: NftMetadata,
    ) -> Result<u64, NftErrorCode> {
        Self::require_minter(&env, &minter)?;
        let _ = metadata;
        Ok(1)
    }

    pub fn mint_reward_nft_from_map(
        env: Env,
        minter: Address,
        _hunt_id: u64,
        _owner: Address,
        values: Map<Symbol, Val>,
    ) -> Result<u64, NftErrorCode> {
        Self::require_minter(&env, &minter)?;
        let _ = values;
        Ok(1)
    }

    pub fn update_nft_metadata(
        env: Env,
        nft_id: u64,
        owner: Address,
        description: String,
        image_uri: String,
    ) -> Result<(), NftErrorCode> {
        // Sanitize the user-supplied description and image_uri
        sanitize_string(&env, &description, MAX_DESCRIPTION_BYTES, true)?;
        // Empty image_uri check (preserved from existing behaviour)
        if image_uri.len() == 0 {
            return Err(NftErrorCode::InvalidImageUri);
        }
        sanitize_string(&env, &image_uri, MAX_NFT_URI_BYTES, false)?;
        let _ = (nft_id, owner);
        Ok(())
    }

    pub fn get_player_nfts(_env: Env, _player: Address, _offset: u32, _limit: u32) -> Vec<u64> {
        Vec::new(&_env)
    }

    pub fn get_nft(_env: Env, _nft_id: u64) -> Option<Nft> {
        None
    }

    pub fn get_schema_version(env: Env) -> u32 {
        migration::NftRewardMigration::get_schema_version(&env)
    }

    pub fn initialize_schema(env: Env, admin: Address) {
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&symbol_short!("ADMIN"))
            .expect("Contract not initialized: no ADMIN set");
        if admin != stored_admin {
            panic!("unauthorized: caller is not the contract admin");
        }
        migration::NftRewardMigration::initialize_schema(&env, &admin);
    }

    pub fn propose_upgrade(
        env: Env,
        admin: Address,
        target_version: u32,
        wasm_hash: BytesN<32>,
    ) -> Result<hunty_migration::UpgradeProposal, hunty_migration::UpgradeAuthError> {
        let proposal = migration::NftRewardMigration::propose_upgrade(
            &env,
            &admin,
            target_version,
            wasm_hash,
        )?;
        env.events().publish(
            migration::NftRewardMigration::upgrade_proposed_topic(&env),
            migration::NftRewardMigration::upgrade_proposed_event(&proposal),
        );
        Ok(proposal)
    }

    pub fn cancel_upgrade(
        env: Env,
        admin: Address,
    ) -> Result<hunty_migration::UpgradeProposal, hunty_migration::UpgradeAuthError> {
        let proposal = migration::NftRewardMigration::cancel_upgrade(&env, &admin)?;
        Ok(proposal)
    }

    pub fn set_upgrade_timelock(
        env: Env,
        admin: Address,
        delay_seconds: u64,
    ) -> Result<(), hunty_migration::UpgradeAuthError> {
        migration::NftRewardMigration::set_upgrade_timelock(&env, &admin, delay_seconds)
    }

    pub fn get_upgrade_proposal(env: Env) -> Option<hunty_migration::UpgradeProposal> {
        migration::NftRewardMigration::get_upgrade_proposal(&env)
    }

    pub fn get_upgrade_timelock(env: Env) -> u64 {
        migration::NftRewardMigration::get_upgrade_timelock(&env)
    }

    pub fn get_upgrade_history(
        env: Env,
        offset: u32,
        limit: u32,
    ) -> soroban_sdk::Vec<hunty_migration::UpgradeHistoryEntry> {
        migration::NftRewardMigration::get_upgrade_history(&env, offset, limit)
    }

    /// Returns a page of schema-migration history entries (no wasm_hash —
    /// migrations are not WASM upgrades).
    pub fn get_migration_history(
        env: Env,
        offset: u32,
        limit: u32,
    ) -> soroban_sdk::Vec<hunty_migration::MigrationHistoryEntry> {
        migration::NftRewardMigration::get_migration_history(&env, offset, limit)
    }

    pub fn upgrade(
        env: Env,
        admin: Address,
        new_wasm_hash: BytesN<32>,
    ) -> Result<(), hunty_migration::UpgradeAuthError> {
        migration::NftRewardMigration::upgrade(&env, &admin, new_wasm_hash)
    }

    pub fn run_migration(
        env: Env,
        admin: Address,
        target_version: u32,
        dry_run: bool,
    ) -> Result<migration::MigrationReport, hunty_migration::UpgradeAuthError> {
        let from_version = migration::NftRewardMigration::get_schema_version(&env);
        let report =
            migration::NftRewardMigration::run_migration(&env, &admin, target_version, dry_run)?;
        if !dry_run && report.succeeded && report.from_version < report.to_version {
            env.events().publish(
                migration::NftRewardMigration::migration_executed_topic(&env),
                migration::NftRewardMigration::migration_executed_event(
                    from_version,
                    report.to_version,
                    env.ledger().timestamp(),
                    admin,
                ),
            );
        }
        Ok(report)
    }

    pub fn rollback_migration(
        env: Env,
        admin: Address,
    ) -> Result<migration::MigrationReport, hunty_migration::UpgradeAuthError> {
        migration::NftRewardMigration::rollback_migration(&env, &admin)
    }
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger as _};
    use soroban_sdk::{IntoVal, Map, Symbol, Val};

    fn collection_metadata(env: &Env) -> CollectionMetadata {
        CollectionMetadata {
            name: String::from_str(env, "Hunty Rewards"),
            description: String::from_str(env, "Reward NFTs for completed hunts"),
            total_supply: 0,
            creator: None,
        }
    }

    fn nft_metadata(env: &Env) -> NftMetadata {
        NftMetadata {
            title: String::from_str(env, "Hunt completion"),
            description: String::from_str(env, "Reward for completing the hunt"),
            image_uri: String::from_str(env, "ipfs://QmTest"),
            hunt_title: String::from_str(env, "Test Hunt"),
            rarity: 0,
            tier: 0,
            creator: None,
            royalty_bps: None,
            extensions: Map::new(env),
        }
    }

    fn minter_values(env: &Env, metadata: &NftMetadata) -> Map<Symbol, Val> {
        let mut values: Map<Symbol, Val> = Map::new(env);
        values.set(
            Symbol::new(env, "title"),
            metadata.title.clone().into_val(env),
        );
        values.set(
            Symbol::new(env, "description"),
            metadata.description.clone().into_val(env),
        );
        values
    }

    /// Deploys the contract through its constructor, which records `minter`
    /// as the only address allowed to mint reward NFTs.
    fn deploy(env: &Env) -> (NftRewardClient<'_>, Address, Address) {
        let admin = Address::generate(env);
        let minter = Address::generate(env);
        let contract_id = env.register(
            NftReward,
            (
                admin.clone(),
                minter.clone(),
                None::<u64>,
                collection_metadata(env),
            ),
        );
        (NftRewardClient::new(env, &contract_id), minter, admin)
    }

    #[test]
    fn mint_reward_nft_rejects_non_minter() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1000);

        let (client, minter, admin) = deploy(&env);
        let stranger = Address::generate(&env);
        let owner = Address::generate(&env);

        // Neither a random caller nor the admin is the whitelisted minter.
        let err = client
            .mint_reward_nft(&stranger, &1u64, &owner, &nft_metadata(&env))
            .unwrap_err();
        assert_eq!(err, NftErrorCode::Unauthorized);

        let err = client
            .mint_reward_nft(&admin, &1u64, &owner, &nft_metadata(&env))
            .unwrap_err();
        assert_eq!(err, NftErrorCode::Unauthorized);

        // The whitelisted minter can still mint.
        assert_eq!(
            client
                .mint_reward_nft(&minter, &1u64, &owner, &nft_metadata(&env))
                .unwrap(),
            1
        );
    }

    #[test]
    fn mint_reward_nft_from_map_rejects_non_minter() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1000);

        let (client, minter, _admin) = deploy(&env);
        let stranger = Address::generate(&env);
        let owner = Address::generate(&env);
        let values = minter_values(&env, &nft_metadata(&env));

        let err = client
            .mint_reward_nft_from_map(&stranger, &2u64, &owner, &values)
            .unwrap_err();
        assert_eq!(err, NftErrorCode::Unauthorized);

        // The whitelisted minter can still mint.
        assert_eq!(
            client
                .mint_reward_nft_from_map(&minter, &2u64, &owner, &values)
                .unwrap(),
            1
        );
    }

    #[test]
    fn set_reward_manager_updates_minter_and_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1000);

        let (client, original_minter, admin) = deploy(&env);
        let reward_manager = Address::generate(&env);
        let owner = Address::generate(&env);

        client
            .set_reward_manager(&admin, &reward_manager)
            .unwrap();

        assert_eq!(
            client.mint_reward_nft(&original_minter, &1, &owner, &nft_metadata(&env)),
            Err(NftErrorCode::Unauthorized)
        );
        assert_eq!(
            client.mint_reward_nft(&reward_manager, &1, &owner, &nft_metadata(&env)),
            Ok(1)
        );
        assert_eq!(env.events().all().events().len(), 1);
    }

    #[test]
    fn set_reward_manager_rejects_non_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _) = deploy(&env);
        let attacker = Address::generate(&env);
        let reward_manager = Address::generate(&env);

        assert!(client
            .try_set_reward_manager(&attacker, &reward_manager)
            .is_err());
    }

    #[test]
    #[should_panic]
    fn set_reward_manager_requires_admin_auth() {
        let env = Env::default();
        let (client, _, admin) = deploy(&env);
        let reward_manager = Address::generate(&env);

        client.set_reward_manager(&admin, &reward_manager);
    }
}
