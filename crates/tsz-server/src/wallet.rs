use std::{collections::BTreeMap, convert::Infallible, io, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use rand10::{rand_core::UnwrapErr, rngs::SysRng};
use secrecy::{ExposeSecret, SecretVec};
use tokio::sync::Mutex;
use zcash_client_backend::{
    data_api::{
        Account as _, AccountBirthday, WalletCommitmentTrees, WalletRead, WalletWrite,
        chain::{BlockSource, ChainState, CommitmentTreeRoot, error, scan_cached_blocks},
        error::Error as WalletError,
        scanning::ScanRange,
        wallet::{
            ConfirmationsPolicy, SpendingKeys, create_proposed_transactions,
            decrypt_and_store_transaction,
            input_selection::{GreedyInputSelector, SpendPolicy, TransparentSpendPolicy},
            propose_shielding_coinbase, propose_standard_transfer_to_address, propose_transfer,
        },
    },
    fees::{DustOutputPolicy, StandardFeeRule, standard::SingleOutputChangeStrategy},
    proto::{
        compact_formats::CompactBlock,
        service::{ChainSpec, RawTransaction, compact_tx_streamer_client::CompactTxStreamerClient},
    },
    wallet::{OvkPolicy, WalletTransparentOutput},
};
use zcash_client_sqlite::{AccountUuid, WalletDb, util::SystemClock, wallet::init::WalletMigrator};
use zcash_keys::{address::Address, encoding::AddressCodec, keys::UnifiedSpendingKey};
use zcash_primitives::block::BlockHash;
use zcash_primitives::merkle_tree::HashSer;
use zcash_primitives::transaction::Transaction;
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::{
    ShieldedPool,
    consensus::{BlockHeight, BranchId},
    local_consensus::LocalNetwork,
    memo::MemoBytes,
    value::Zatoshis,
};
use zip321::{Payment, TransactionRequest};

use transparent::{
    address::Script,
    bundle::{OutPoint, TxOut},
};
use zcash_script::script;

use crate::db::Account;
use crate::rpc::ChainCheckpoint;

mod recovery;

type Db = WalletDb<rusqlite::Connection, LocalNetwork, SystemClock, UnwrapErr<SysRng>>;

pub(crate) const WALLET_BIRTHDAY_HEIGHT: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("insufficient spendable funds (have {available}, need {required} including fees)")]
    InsufficientFunds { available: u64, required: u64 },
    #[error("faucet treasury remains insufficient after replenishment")]
    TreasuryExhausted,
    #[error("memos can only be sent to the orchard pool; transparent outputs cannot carry a memo")]
    TransparentMemo,
    #[error("wallet requires reconciliation before constructing a payment")]
    ScanRequired,
}

#[derive(Clone, Debug)]
pub(crate) struct TransparentQuery {
    pub account_id: AccountUuid,
    pub addresses: Vec<String>,
    pub start_height: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct TransparentOutputData {
    pub txid: [u8; 32],
    pub index: u32,
    pub value_zatoshi: i64,
    pub script: Vec<u8>,
    pub height: u32,
    pub account_id: AccountUuid,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ShieldedProtocol {
    Sapling,
    Orchard,
    Ironwood,
}

#[derive(Clone, Debug)]
pub(crate) struct SubtreeRootData {
    pub completing_height: u32,
    pub root_hash: Vec<u8>,
}

pub(crate) enum ScanOutcome {
    Scanned(bool),
    Rewind(u32),
}

#[derive(Clone)]
pub struct RealWallet {
    db: Arc<Mutex<Db>>,
    account_ids: Vec<AccountUuid>,
    lightwalletd: String,
}

struct MemoryBlockCache(BTreeMap<u32, CompactBlock>);

impl BlockSource for MemoryBlockCache {
    type Error = io::Error;

    fn with_blocks<F, E>(
        &self,
        from: Option<BlockHeight>,
        limit: Option<usize>,
        mut f: F,
    ) -> Result<(), error::Error<E, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), error::Error<E, Self::Error>>,
    {
        let from = from.map(u32::from).unwrap_or(0);
        for block in self.0.range(from..).take(limit.unwrap_or(usize::MAX)) {
            f(block.1.clone())?;
        }
        Ok(())
    }
}

pub fn regtest_network() -> LocalNetwork {
    let one = Some(BlockHeight::from_u32(1));
    LocalNetwork {
        overwinter: one,
        sapling: one,
        blossom: one,
        heartwood: one,
        canopy: one,
        nu5: one,
        nu6: one,
        nu6_1: None,
        nu6_2: None,
        nu6_3: None,
    }
}

impl RealWallet {
    pub fn open(data_dir: &Path, seed_hex: &str) -> Result<Self> {
        let seed = hex::decode(seed_hex).context("invalid wallet seed")?;
        let secret = SecretVec::new(seed);
        let wallet_path = data_dir.join("wallet.db");
        let mut db = WalletDb::for_path(
            wallet_path,
            regtest_network(),
            SystemClock,
            UnwrapErr(SysRng),
        )?;
        WalletMigrator::new()
            .with_seed(SecretVec::new(secret.expose_secret().clone()))
            .with_external_migrations(vec![Box::new(recovery::TreasuryCursorMigration)])
            .init_or_migrate(&mut db)
            .map_err(|e| anyhow::anyhow!("initializing wallet database: {e}"))?;

        let account_count = db.get_account_ids()?.len();
        if account_count == 0 || account_count == usize::from(crate::db::USER_ACCOUNT_COUNT) {
            // lightwalletd treats a BlockId with height 0 as unspecified, while the
            // SDK asks for the tree state immediately before an account birthday.
            // Start at block 2 so that the initial tree-state request is for block 1.
            // Block 1 is an expendable mining-reward block on this local regtest.
            let birthday = AccountBirthday::from_parts(
                ChainState::empty(
                    BlockHeight::from_u32(WALLET_BIRTHDAY_HEIGHT - 1),
                    BlockHash([0; 32]),
                ),
                None,
            );
            for id in (account_count + 1)..=usize::from(crate::db::TREASURY_ACCOUNT_ID) {
                db.create_account(&format!("Account {id}"), &secret, &birthday, None)?;
            }
        }
        if db.get_account_ids()?.len() != usize::from(crate::db::TREASURY_ACCOUNT_ID) {
            bail!("wallet database must contain exactly six accounts");
        }
        let mut accounts = db
            .get_account_ids()?
            .into_iter()
            .map(|id| {
                db.get_account(id)?
                    .map(|account| (account.name().unwrap_or_default().to_owned(), id))
                    .context("wallet account disappeared")
            })
            .collect::<Result<Vec<_>>>()?;
        accounts.sort_by(|a, b| a.0.cmp(&b.0));
        let account_ids = accounts.into_iter().map(|(_, id)| id).collect();

        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            account_ids,
            lightwalletd: std::env::var("TSZ_LIGHTWALLETD")
                .unwrap_or_else(|_| "http://127.0.0.1:9067".into()),
        })
    }

    pub(crate) fn lightwalletd(&self) -> &str {
        &self.lightwalletd
    }

    async fn with_db<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Db) -> Result<T> + Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || operation(&mut db.blocking_lock()))
            .await
            .context("wallet worker stopped")?
    }

    pub(crate) async fn public_transparent_queries(&self) -> Result<Vec<TransparentQuery>> {
        let account_ids = self.account_ids[..usize::from(crate::db::USER_ACCOUNT_COUNT)].to_vec();
        self.with_db(move |db| {
            account_ids
                .into_iter()
                .map(|account_id| {
                    Ok(TransparentQuery {
                        account_id,
                        addresses: db
                            .get_transparent_receivers(account_id, true, true)?
                            .into_keys()
                            .map(|address| address.encode(&regtest_network()))
                            .collect(),
                        start_height: u64::from(u32::from(db.utxo_query_height(account_id)?)),
                    })
                })
                .collect()
        })
        .await
    }

    pub(crate) async fn insert_transparent_outputs(
        &self,
        outputs: Vec<TransparentOutputData>,
    ) -> Result<()> {
        self.with_db(move |db| {
            for output in outputs {
                let output = WalletTransparentOutput::from_parts(
                    OutPoint::new(output.txid, output.index),
                    TxOut::new(
                        Zatoshis::from_nonnegative_i64(output.value_zatoshi)
                            .map_err(|_| anyhow::anyhow!("negative transparent output value"))?,
                        Script(script::Code(output.script)),
                    ),
                    Some(BlockHeight::from_u32(output.height)),
                    Some(output.account_id),
                    None,
                    None,
                )
                .context("invalid transparent output")?;
                db.put_received_transparent_utxo(&output)?;
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn put_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        roots: Vec<SubtreeRootData>,
    ) -> Result<()> {
        self.with_db(move |db| {
            match protocol {
                ShieldedProtocol::Sapling => {
                    let roots = roots
                        .into_iter()
                        .map(|root| {
                            Ok(CommitmentTreeRoot::from_parts(
                                BlockHeight::from_u32(root.completing_height),
                                sapling::Node::read(&root.root_hash[..])?,
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    db.put_sapling_subtree_roots(0, &roots)?;
                }
                ShieldedProtocol::Orchard | ShieldedProtocol::Ironwood => {
                    let roots = roots
                        .into_iter()
                        .map(|root| {
                            Ok(CommitmentTreeRoot::from_parts(
                                BlockHeight::from_u32(root.completing_height),
                                orchard::tree::MerkleHashOrchard::read(&root.root_hash[..])?,
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    match protocol {
                        ShieldedProtocol::Orchard => db.put_orchard_subtree_roots(0, &roots)?,
                        ShieldedProtocol::Ironwood => db.put_ironwood_subtree_roots(0, &roots)?,
                        ShieldedProtocol::Sapling => unreachable!(),
                    }
                }
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn update_chain_tip(&self, height: u64) -> Result<()> {
        let height = u32::try_from(height).context("chain height exceeds the wallet range")?;
        self.with_db(move |db| {
            db.update_chain_tip(BlockHeight::from_u32(height))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn suggested_scan_ranges(&self) -> Result<Vec<ScanRange>> {
        self.with_db(|db| db.suggest_scan_ranges().map_err(Into::into))
            .await
    }

    pub(crate) async fn scan_batch(
        &self,
        range: ScanRange,
        blocks: Vec<CompactBlock>,
        chain_state: ChainState,
    ) -> Result<ScanOutcome> {
        self.with_db(move |db| {
            let cache = MemoryBlockCache(
                blocks
                    .into_iter()
                    .map(|block| (block.height as u32, block))
                    .collect(),
            );
            match scan_cached_blocks(
                &regtest_network(),
                &cache,
                db,
                range.block_range().start,
                &chain_state,
                range.len(),
            ) {
                Err(error::Error::Scan(error)) if error.is_continuity_error() => {
                    Ok(ScanOutcome::Rewind(
                        u32::from(error.at_height().saturating_sub(10))
                            .max(WALLET_BIRTHDAY_HEIGHT - 1),
                    ))
                }
                Ok(_) => Ok(ScanOutcome::Scanned(
                    db.suggest_scan_ranges()?
                        .first()
                        .is_some_and(|latest| latest.priority() > range.priority()),
                )),
                Err(error) => Err(anyhow::anyhow!("scanning compact blocks: {error}")),
            }
        })
        .await
    }

    pub(crate) async fn rewind_to_height(&self, chain_state: ChainState) -> Result<()> {
        self.with_db(move |db| {
            db.transactionally_with_extension::<_, _, anyhow::Error>(|wallet, ext| {
                let height = u32::from(chain_state.block_height());
                let hash = chain_state.block_hash().to_string();
                wallet.truncate_to_chain_state(chain_state)?;
                ext.execute(
                    "UPDATE ext_tsz_treasury_cursor SET height=?1,block_hash=?2,updated_at=CURRENT_TIMESTAMP WHERE height>=?1",
                    rusqlite::params![height, hash],
                )?;
                Ok(())
            })
        })
        .await
    }

    pub(crate) async fn block_hash(&self, height: u32) -> Result<Option<String>> {
        self.with_db(move |db| {
            Ok(db
                .get_block_hash(height.into())?
                .map(|hash| hash.to_string()))
        })
        .await
    }

    pub(crate) async fn max_scanned_checkpoint(&self) -> Result<Option<ChainCheckpoint>> {
        self.with_db(|db| {
            Ok(db.block_max_scanned()?.map(|block| ChainCheckpoint {
                height: u64::from(u32::from(block.block_height())),
                hash: block.block_hash().to_string(),
            }))
        })
        .await
    }

    pub(crate) async fn scanned_checkpoint(&self) -> Result<Option<ChainCheckpoint>> {
        self.with_db(|db| {
            Ok(db.block_fully_scanned()?.map(|block| ChainCheckpoint {
                height: u64::from(u32::from(block.block_height())),
                hash: block.block_hash().to_string(),
            }))
        })
        .await
    }

    pub async fn wait_for_height(&self, target: u64, timeout: Duration) -> Result<()> {
        let mut latest_indexed = None;
        let wait = async {
            let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
            loop {
                let indexed = client
                    .get_latest_block(ChainSpec::default())
                    .await?
                    .into_inner()
                    .height;
                latest_indexed = Some(indexed);
                if indexed >= target {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        };
        if let Ok(result) = tokio::time::timeout(timeout, wait).await {
            return result;
        }
        let latest_indexed =
            latest_indexed.map_or_else(|| "unknown".into(), |height| height.to_string());
        bail!(
            "lightwalletd did not index Zakura height {target} within {} seconds (latest indexed height: {latest_indexed})",
            timeout.as_secs()
        );
    }

    pub async fn apply_balances(&self, accounts: &mut [Account]) -> Result<()> {
        let Some(summary) = self
            .with_db(|db| Ok(db.get_wallet_summary(ConfirmationsPolicy::MIN)?))
            .await?
        else {
            return Ok(());
        };
        for (account, wallet_id) in accounts.iter_mut().zip(&self.account_ids) {
            if let Some(balance) = summary.account_balances().get(wallet_id) {
                account.transparent_zatoshi = u64::from(balance.unshielded_balance().total());
                account.orchard_zatoshi = u64::from(balance.orchard_balance().total());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub async fn enhance_transaction(&self, raw_hex: &str, height: u32) -> Result<()> {
        let raw_hex = raw_hex.to_owned();
        self.with_db(move |db| {
            let params = regtest_network();
            let height = BlockHeight::from_u32(height);
            let raw = hex::decode(raw_hex).context("invalid transaction hex from Zakura")?;
            let tx = Transaction::read(&raw[..], BranchId::for_height(&params, height))
                .context("parsing transaction from Zakura")?;
            decrypt_and_store_transaction(&params, db, &tx, Some(height))?;
            Ok(())
        })
        .await
    }

    pub async fn send(
        &self,
        seed_hex: &str,
        from_account: u8,
        source_pool: &str,
        destination: &str,
        amount: u64,
        memo: Option<MemoBytes>,
    ) -> Result<String> {
        let account_index = from_account
            .checked_sub(1)
            .context("invalid source account")?;
        let mut db = self.db.lock().await;
        let account_id = *self
            .account_ids
            .get(account_index as usize)
            .context("source account does not exist")?;
        let params = regtest_network();
        let recipient =
            Address::decode(&params, destination).context("invalid destination address")?;
        if memo.is_some() && matches!(recipient, Address::Transparent(_) | Address::Tex(_)) {
            return Err(PaymentError::TransparentMemo.into());
        }
        let amount = Zatoshis::from_u64(amount).map_err(|_| anyhow::anyhow!("invalid amount"))?;
        let proposal = if source_pool == "transparent" {
            let request = TransactionRequest::new(vec![Payment::new(
                recipient.to_zcash_address(&params),
                Some(amount),
                memo,
                None,
                None,
                vec![],
            )?])?;
            let selector = GreedyInputSelector::<Db>::new();
            let change = SingleOutputChangeStrategy::<Db>::new(
                StandardFeeRule::Zip317,
                None,
                ShieldedPool::Orchard,
                DustOutputPolicy::default(),
            );
            let policy = SpendPolicy::shielded_pools([])
                .with_transparent(TransparentSpendPolicy::any_account_addr());
            propose_transfer::<_, _, _, _, Infallible>(
                &mut *db,
                &params,
                account_id,
                &selector,
                &change,
                request,
                ConfirmationsPolicy::MIN,
                &policy,
                None,
                None,
            )
        } else if source_pool == "orchard" {
            propose_standard_transfer_to_address::<_, _, Infallible>(
                &mut *db,
                &params,
                StandardFeeRule::Zip317,
                account_id,
                ConfirmationsPolicy::MIN,
                &recipient,
                amount,
                memo,
                None,
                ShieldedPool::Orchard,
                None,
                None,
            )
        } else {
            bail!("source pool must be transparent or orchard")
        }
        .map_err(|error| match error {
            WalletError::InsufficientFunds {
                available,
                required,
            } => anyhow::Error::new(PaymentError::InsufficientFunds {
                available: u64::from(available),
                required: u64::from(required),
            }),
            WalletError::ScanRequired => anyhow::Error::new(PaymentError::ScanRequired),
            error => anyhow::anyhow!("proposing {source_pool} transaction: {error}"),
        })?;
        let seed = hex::decode(seed_hex)?;
        let usk = UnifiedSpendingKey::from_seed(
            &params,
            &seed,
            zip32::AccountId::try_from(u32::from(account_index))
                .map_err(|_| anyhow::anyhow!("invalid account index"))?,
        )
        .map_err(|e| anyhow::anyhow!("deriving spending key: {e:?}"))?;
        let prover = LocalTxProver::bundled();
        let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
            &mut *db,
            &params,
            &prover,
            &prover,
            &SpendingKeys::from_unified_spending_key(usk),
            OvkPolicy::Sender,
            &proposal,
            None,
        )
        .map_err(|e| anyhow::anyhow!("building transaction: {e}"))?;
        let txid = *txids.first();
        let tx = db
            .get_transaction(txid)?
            .context("built transaction was not stored")?;
        let mut raw = vec![];
        tx.write(&mut raw)?;
        drop(db);
        let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
        let result = client
            .send_transaction(RawTransaction {
                data: raw,
                height: 0,
            })
            .await?
            .into_inner();
        if result.error_code != 0 {
            bail!(
                "lightwalletd rejected transaction: {}",
                result.error_message
            );
        }
        Ok(txid.to_string())
    }

    pub async fn shield_coinbase(
        &self,
        seed_hex: &str,
        treasury_account: u8,
        from: &str,
        to: &str,
        minimum_net: u64,
    ) -> Result<String> {
        if treasury_account != crate::db::TREASURY_ACCOUNT_ID {
            bail!("coinbase shielding requires the treasury account");
        }
        let seed_hex = seed_hex.to_owned();
        let from = from.to_owned();
        let to = to.to_owned();
        let (txid, raw) = self
            .with_db(move |db| {
                let params = regtest_network();
                let from =
                    match Address::decode(&params, &from).context("invalid treasury address")? {
                        Address::Transparent(address) => address,
                        _ => bail!("treasury address is not transparent"),
                    };
                let to = Address::decode(&params, &to)
                    .context("invalid shielding address")?
                    .to_zcash_address(&params);
                let threshold = Zatoshis::from_u64(minimum_net)
                    .map_err(|_| anyhow::anyhow!("invalid shielding threshold"))?;
                // Existing wallets may already hold thousands of rewards. Grow the SDK's
                // input cap only until its fee-aware proposal covers this payment.
                let mut limit = Some(1);
                let proposal = loop {
                    match propose_shielding_coinbase::<_, _, _, _, Infallible>(
                        &mut *db,
                        &params,
                        &GreedyInputSelector::new(),
                        &StandardFeeRule::Zip317,
                        threshold,
                        &[from],
                        to.clone(),
                        None,
                        limit,
                        None,
                    ) {
                        Ok(proposal) => break proposal,
                        Err(WalletError::InsufficientFunds { .. }) if limit.is_some() => {
                            limit = limit.filter(|value| *value < 2048).map(|value| value * 2);
                        }
                        Err(WalletError::InsufficientFunds { .. } | WalletError::ScanRequired) => {
                            return Err(anyhow::Error::new(PaymentError::TreasuryExhausted));
                        }
                        Err(error) => {
                            return Err(anyhow::anyhow!("proposing coinbase shielding: {error}"));
                        }
                    }
                };
                let seed = hex::decode(seed_hex)?;
                let account_index = treasury_account
                    .checked_sub(1)
                    .context("invalid treasury account")?;
                let usk = UnifiedSpendingKey::from_seed(
                    &params,
                    &seed,
                    zip32::AccountId::try_from(u32::from(account_index))
                        .map_err(|_| anyhow::anyhow!("invalid treasury account"))?,
                )
                .map_err(|e| anyhow::anyhow!("deriving treasury key: {e:?}"))?;
                let prover = LocalTxProver::bundled();
                let txids = create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
                    &mut *db,
                    &params,
                    &prover,
                    &prover,
                    &SpendingKeys::from_unified_spending_key(usk),
                    OvkPolicy::Sender,
                    &proposal,
                    None,
                )
                .map_err(|e| anyhow::anyhow!("building shielding transaction: {e}"))?;
                let txid = *txids.first();
                let tx = db
                    .get_transaction(txid)?
                    .context("shielding transaction was not stored")?;
                let mut raw = vec![];
                tx.write(&mut raw)?;
                Ok((txid.to_string(), raw))
            })
            .await?;
        let mut client = CompactTxStreamerClient::connect(self.lightwalletd.clone()).await?;
        let response = client
            .send_transaction(RawTransaction {
                data: raw,
                height: 0,
            })
            .await?
            .into_inner();
        if response.error_code != 0 {
            bail!(
                "lightwalletd rejected shielding: {}",
                response.error_message
            );
        }
        Ok(txid)
    }
}

#[cfg(test)]
mod treasury_sync_tests {
    use super::*;
    use crate::db::{Store, TREASURY_ACCOUNT_ID, USER_ACCOUNT_COUNT};

    #[tokio::test]
    async fn wallet_migration_installs_only_the_treasury_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let seed = SecretVec::new(hex::decode(store.seed().unwrap()).unwrap());
        let mut db = WalletDb::for_path(
            dir.path().join("wallet.db"),
            regtest_network(),
            SystemClock,
            UnwrapErr(SysRng),
        )
        .unwrap();
        zcash_client_sqlite::wallet::init::init_wallet_db(
            &mut db,
            Some(SecretVec::new(seed.expose_secret().clone())),
        )
        .unwrap();
        let birthday =
            AccountBirthday::from_parts(ChainState::empty(1.into(), BlockHash([0; 32])), None);
        for id in 1..=USER_ACCOUNT_COUNT {
            db.create_account(&format!("Account {id}"), &seed, &birthday, None)
                .unwrap();
        }
        drop(db);

        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        wallet
            .with_db(|db| {
                assert_eq!(db.get_account_ids()?.len(), 6);
                let (cursor, prepared): (u32, u32) =
                    db.transactionally_with_extension::<_, _, anyhow::Error>(|_, ext| {
                        Ok((
                            ext.query_row(
                                "SELECT COUNT(*) FROM sqlite_master WHERE name='ext_tsz_treasury_cursor'",
                                [],
                                |row| row.get(0),
                            )?,
                            ext.query_row(
                                "SELECT COUNT(*) FROM sqlite_master WHERE name='ext_tsz_prepared_transactions'",
                                [],
                                |row| row.get(0),
                            )?,
                        ))
                    })?;
                assert_eq!((cursor, prepared), (1, 0));
                Ok(())
            })
            .await
            .unwrap();
        drop(wallet);
        let reopened = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        assert!(reopened.treasury_cursor().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn routine_transparent_queries_exclude_the_treasury() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();

        let queries = wallet.public_transparent_queries().await.unwrap();
        assert_eq!(queries.len(), usize::from(USER_ACCOUNT_COUNT));
        for id in 1..=USER_ACCOUNT_COUNT {
            assert!(queries.iter().any(|query| {
                query
                    .addresses
                    .contains(&store.account(id).unwrap().transparent_address)
            }));
        }
        assert!(!queries.iter().any(|query| {
            query.addresses.contains(
                &store
                    .account(TREASURY_ACCOUNT_ID)
                    .unwrap()
                    .transparent_address,
            )
        }));
    }

    #[tokio::test]
    async fn height_wait_deadline_includes_stalled_lightwalletd_calls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let mut wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        wallet.lightwalletd = format!("http://{}", listener.local_addr().unwrap());

        let result = tokio::time::timeout(
            Duration::from_millis(250),
            wallet.wait_for_height(1, Duration::from_millis(50)),
        )
        .await;

        assert!(result.is_ok(), "height wait ignored its deadline");
        assert!(result.unwrap().is_err());
    }

    #[tokio::test]
    async fn database_work_does_not_block_the_async_executor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("server.db")).unwrap();
        store.initialize().unwrap();
        let wallet = RealWallet::open(dir.path(), &store.seed().unwrap()).unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        let work = tokio::spawn(async move {
            wallet
                .with_db(move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(())
                })
                .await
        });
        entered_rx.await.unwrap();
        let elapsed = started.elapsed();
        release_tx.send(()).unwrap();
        work.await.unwrap().unwrap();
        assert!(
            elapsed < Duration::from_millis(750),
            "database work blocked the async executor for {elapsed:?}"
        );
    }
}
