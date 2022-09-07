use crate::blaze::fetch_full_transaction::TransactionContext;
use crate::compact_formats::TreeState;
use crate::wallet::data::TransactionMetadata;
use crate::wallet::keys::transparent::TransparentKey;
use crate::wallet::{data::SpendableSaplingNote, keys::sapling::SaplingKey};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use futures::Future;
use log::{error, info, warn};
use orchard::keys::SpendingKey as OrchardSpendingKey;
use orchard::note_encryption::OrchardDomain;
use std::{
    cmp,
    collections::HashMap,
    io::{self, Error, ErrorKind, Read, Write},
    sync::{atomic::AtomicU64, mpsc::channel, Arc},
    time::SystemTime,
};
use tokio::sync::RwLock;
use zcash_client_backend::{
    address,
    encoding::{
        decode_extended_full_viewing_key, decode_extended_spending_key, encode_payment_address,
    },
};
use zcash_encoding::{Optional, Vector};
use zcash_note_encryption::Domain;
use zcash_primitives::memo::MemoBytes;
use zcash_primitives::merkle_tree::CommitmentTree;
use zcash_primitives::sapling::note_encryption::SaplingDomain;
use zcash_primitives::transaction::builder::Progress;
use zcash_primitives::{
    consensus::BlockHeight,
    legacy::Script,
    memo::Memo,
    sapling::prover::TxProver,
    transaction::{
        builder::Builder,
        components::{amount::DEFAULT_FEE, Amount, OutPoint, TxOut},
    },
};

use self::data::SpendableOrchardNote;
use self::traits::{DomainWalletExt, NoteAndMetadata, SpendableNote};
use self::{
    data::{BlockData, OrchardNoteAndMetadata, SaplingNoteAndMetadata, Utxo, WalletZecPriceInfo},
    keys::{orchard::OrchardKey, Keys},
    message::Message,
    transactions::TransactionMetadataSet,
};
use zingoconfig::ZingoConfig;

pub(crate) mod data;
pub(crate) mod keys;
pub(crate) mod message;
pub(crate) mod traits;
pub(crate) mod transactions;
pub(crate) mod utils;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[derive(Debug, Clone)]
pub struct SendProgress {
    pub id: u32,
    pub is_send_in_progress: bool,
    pub progress: u32,
    pub total: u32,
    pub last_error: Option<String>,
    pub last_transaction_id: Option<String>,
}

impl SendProgress {
    fn new(id: u32) -> Self {
        SendProgress {
            id,
            is_send_in_progress: false,
            progress: 0,
            total: 0,
            last_error: None,
            last_transaction_id: None,
        }
    }
}

// Enum to refer to the first or last position of the Node
pub enum NodePosition {
    Oldest,
    Highest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoDownloadOption {
    NoMemos = 0,
    WalletMemos,
    AllMemos,
}

#[derive(Debug, Clone, Copy)]
pub struct WalletOptions {
    pub(crate) download_memos: MemoDownloadOption,
}

impl Default for WalletOptions {
    fn default() -> Self {
        WalletOptions {
            download_memos: MemoDownloadOption::WalletMemos,
        }
    }
}

impl WalletOptions {
    pub fn serialized_version() -> u64 {
        return 1;
    }

    pub fn read<R: Read>(mut reader: R) -> io::Result<Self> {
        let _version = reader.read_u64::<LittleEndian>()?;

        let download_memos = match reader.read_u8()? {
            0 => MemoDownloadOption::NoMemos,
            1 => MemoDownloadOption::WalletMemos,
            2 => MemoDownloadOption::AllMemos,
            v => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Bad download option {}", v),
                ));
            }
        };

        Ok(Self { download_memos })
    }

    pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        // Write the version
        writer.write_u64::<LittleEndian>(Self::serialized_version())?;

        writer.write_u8(self.download_memos as u8)
    }
}

pub struct LightWallet {
    // The block at which this wallet was born. Rescans
    // will start from here.
    birthday: AtomicU64,

    // The last 100 blocks, used if something gets re-orged
    pub(super) blocks: Arc<RwLock<Vec<BlockData>>>,

    // Wallet options
    pub(crate) wallet_options: Arc<RwLock<WalletOptions>>,

    // Heighest verified block
    pub(crate) verified_tree: Arc<RwLock<Option<TreeState>>>,

    // Progress of an outgoing transaction
    send_progress: Arc<RwLock<SendProgress>>,

    // The current price of ZEC. (time_fetched, price in USD)
    pub price: Arc<RwLock<WalletZecPriceInfo>>,

    // Local data to the proxy to specify transactions to fetch.
    pub(crate) transaction_context: TransactionContext,
}

use crate::wallet::traits::{Diversifiable as _, WalletKey};
impl LightWallet {
    pub fn serialized_version() -> u64 {
        return 24;
    }

    pub fn new(
        config: ZingoConfig,
        seed_phrase: Option<String>,
        height: u64,
        num_zaddrs: u32,
    ) -> io::Result<Self> {
        let keys = Keys::new(&config, seed_phrase, num_zaddrs)
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;

        let transaction_metadata_set = Arc::new(RwLock::new(TransactionMetadataSet::new()));
        let transaction_context = TransactionContext::new(
            &config,
            Arc::new(RwLock::new(keys)),
            transaction_metadata_set,
        );
        Ok(Self {
            blocks: Arc::new(RwLock::new(vec![])),
            wallet_options: Arc::new(RwLock::new(WalletOptions::default())),
            birthday: AtomicU64::new(height),
            verified_tree: Arc::new(RwLock::new(None)),
            send_progress: Arc::new(RwLock::new(SendProgress::new(0))),
            price: Arc::new(RwLock::new(WalletZecPriceInfo::new())),
            transaction_context,
        })
    }

    pub async fn read<R: Read>(mut reader: R, config: &ZingoConfig) -> io::Result<Self> {
        let version = reader.read_u64::<LittleEndian>()?;
        if version > Self::serialized_version() {
            let e = format!(
                "Don't know how to read wallet version {}. Do you have the latest version?",
                version
            );
            error!("{}", e);
            return Err(io::Error::new(ErrorKind::InvalidData, e));
        }

        info!("Reading wallet version {}", version);

        let keys = if version <= 14 {
            Keys::read_old(version, &mut reader, config)
        } else {
            Keys::read(&mut reader, config)
        }?;

        let mut blocks = Vector::read(&mut reader, |r| BlockData::read(r))?;
        if version <= 14 {
            // Reverse the order, since after version 20, we need highest-block-first
            blocks = blocks.into_iter().rev().collect();
        }

        let mut transactions = if version <= 14 {
            TransactionMetadataSet::read_old(&mut reader)
        } else {
            TransactionMetadataSet::read(&mut reader)
        }?;

        let chain_name = utils::read_string(&mut reader)?;

        if chain_name != config.chain.to_string() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Wallet chain name {} doesn't match expected {}",
                    chain_name, config.chain
                ),
            ));
        }

        let wallet_options = if version <= 23 {
            WalletOptions::default()
        } else {
            WalletOptions::read(&mut reader)?
        };

        let birthday = reader.read_u64::<LittleEndian>()?;

        if version <= 22 {
            let _sapling_tree_verified = if version <= 12 {
                true
            } else {
                reader.read_u8()? == 1
            };
        }

        let verified_tree = if version <= 21 {
            None
        } else {
            Optional::read(&mut reader, |r| {
                use prost::Message;

                let buf = Vector::read(r, |r| r.read_u8())?;
                TreeState::decode(&buf[..]).map_err(|e| {
                    io::Error::new(
                        ErrorKind::InvalidData,
                        format!("Read Error: {}", e.to_string()),
                    )
                })
            })?
        };

        // If version <= 8, adjust the "is_spendable" status of each note data
        if version <= 8 {
            // Collect all spendable keys
            let spendable_keys: Vec<_> = keys
                .get_all_sapling_extfvks()
                .into_iter()
                .filter(|extfvk| keys.have_sapling_spending_key(extfvk))
                .collect();

            transactions.adjust_spendable_status(spendable_keys);
        }

        let price = if version <= 13 {
            WalletZecPriceInfo::new()
        } else {
            WalletZecPriceInfo::read(&mut reader)?
        };

        let transaction_context = TransactionContext::new(
            &config,
            Arc::new(RwLock::new(keys)),
            Arc::new(RwLock::new(transactions)),
        );
        let mut lw = Self {
            blocks: Arc::new(RwLock::new(blocks)),
            wallet_options: Arc::new(RwLock::new(wallet_options)),
            birthday: AtomicU64::new(birthday),
            verified_tree: Arc::new(RwLock::new(verified_tree)),
            send_progress: Arc::new(RwLock::new(SendProgress::new(0))),
            price: Arc::new(RwLock::new(price)),
            transaction_context,
        };

        // For old wallets, remove unused addresses
        if version <= 14 {
            lw.remove_unused_taddrs().await;
            lw.remove_unused_zaddrs().await;
        }

        if version <= 14 {
            lw.set_witness_block_heights().await;
        }

        Ok(lw)
    }

    pub async fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        if self.transaction_context.keys.read().await.encrypted
            && self.transaction_context.keys.read().await.unlocked
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("Cannot write while wallet is unlocked while encrypted."),
            ));
        }

        // Write the version
        writer.write_u64::<LittleEndian>(Self::serialized_version())?;

        // Write all the keys
        self.transaction_context
            .keys
            .read()
            .await
            .write(&mut writer)?;

        Vector::write(&mut writer, &self.blocks.read().await, |w, b| b.write(w))?;

        self.transaction_context
            .transaction_metadata_set
            .read()
            .await
            .write(&mut writer)?;

        utils::write_string(
            &mut writer,
            &self.transaction_context.config.chain.to_string(),
        )?;

        self.wallet_options.read().await.write(&mut writer)?;

        // While writing the birthday, get it from the fn so we recalculate it properly
        // in case of rescans etc...
        writer.write_u64::<LittleEndian>(self.get_birthday().await)?;

        Optional::write(
            &mut writer,
            self.verified_tree.read().await.as_ref(),
            |w, t| {
                use prost::Message;
                let mut buf = vec![];

                t.encode(&mut buf)?;
                Vector::write(w, &buf, |w, b| w.write_u8(*b))
            },
        )?;

        // Price info
        self.price.read().await.write(&mut writer)?;

        Ok(())
    }

    // Before version 20, witnesses didn't store their height, so we need to update them.
    pub async fn set_witness_block_heights(&mut self) {
        let top_height = self.last_scanned_height().await;
        self.transaction_context
            .transaction_metadata_set
            .write()
            .await
            .current
            .iter_mut()
            .for_each(|(_, wtx)| {
                wtx.sapling_notes.iter_mut().for_each(|nd| {
                    nd.witnesses.top_height = top_height;
                });
            });
    }

    pub fn keys(&self) -> Arc<RwLock<Keys>> {
        self.transaction_context.keys.clone()
    }

    pub fn transactions(&self) -> Arc<RwLock<TransactionMetadataSet>> {
        self.transaction_context.transaction_metadata_set.clone()
    }

    pub async fn set_blocks(&self, new_blocks: Vec<BlockData>) {
        let mut blocks = self.blocks.write().await;
        blocks.clear();
        blocks.extend_from_slice(&new_blocks[..]);
    }

    /// Return a copy of the blocks currently in the wallet, needed to process possible reorgs
    pub async fn get_blocks(&self) -> Vec<BlockData> {
        self.blocks.read().await.iter().map(|b| b.clone()).collect()
    }

    pub(crate) fn note_address<NnMd: NoteAndMetadata>(
        network: &zingoconfig::Network,
        note: &NnMd,
    ) -> Option<String> {
        note.fvk()
            .diversified_address(*note.diversifier())
            .map(|address| traits::Recipient::b32encode_for_network(&address, network))
    }

    pub async fn set_download_memo(&self, value: MemoDownloadOption) {
        self.wallet_options.write().await.download_memos = value;
    }

    pub async fn get_birthday(&self) -> u64 {
        let birthday = self.birthday.load(std::sync::atomic::Ordering::SeqCst);
        if birthday == 0 {
            self.get_first_transaction_block().await
        } else {
            cmp::min(self.get_first_transaction_block().await, birthday)
        }
    }

    pub async fn set_latest_zec_price(&self, price: f64) {
        if price <= 0 as f64 {
            warn!("Tried to set a bad current zec price {}", price);
            return;
        }

        self.price.write().await.zec_price = Some((now(), price));
        info!("Set current ZEC Price to USD {}", price);
    }

    // Get the current sending status.
    pub async fn get_send_progress(&self) -> SendProgress {
        self.send_progress.read().await.clone()
    }

    // Set the previous send's status as an error
    async fn set_send_error(&self, e: String) {
        let mut p = self.send_progress.write().await;

        p.is_send_in_progress = false;
        p.last_error = Some(e);
    }

    // Set the previous send's status as success
    async fn set_send_success(&self, transaction_id: String) {
        let mut p = self.send_progress.write().await;

        p.is_send_in_progress = false;
        p.last_transaction_id = Some(transaction_id);
    }

    // Reset the send progress status to blank
    async fn reset_send_progress(&self) {
        let mut g = self.send_progress.write().await;
        let next_id = g.id + 1;

        // Discard the old value, since we are replacing it
        let _ = std::mem::replace(&mut *g, SendProgress::new(next_id));
    }

    pub async fn is_unlocked_for_spending(&self) -> bool {
        self.transaction_context
            .keys
            .read()
            .await
            .is_unlocked_for_spending()
    }

    pub async fn is_encrypted(&self) -> bool {
        self.transaction_context.keys.read().await.is_encrypted()
    }

    // Get the first block that this wallet has a transaction in. This is often used as the wallet's "birthday"
    // If there are no transactions, then the actual birthday (which is recorder at wallet creation) is returned
    // If no birthday was recorded, return the sapling activation height
    pub async fn get_first_transaction_block(&self) -> u64 {
        // Find the first transaction
        let earliest_block = self
            .transaction_context
            .transaction_metadata_set
            .read()
            .await
            .current
            .values()
            .map(|wtx| u64::from(wtx.block))
            .min();

        let birthday = self.birthday.load(std::sync::atomic::Ordering::SeqCst);
        earliest_block // Returns optional, so if there's no transactions, it'll get the activation height
            .unwrap_or(cmp::max(
                birthday,
                self.transaction_context.config.sapling_activation_height(),
            ))
    }

    fn adjust_wallet_birthday(&self, new_birthday: u64) {
        let mut wallet_birthday = self.birthday.load(std::sync::atomic::Ordering::SeqCst);
        if new_birthday < wallet_birthday {
            wallet_birthday = cmp::max(
                new_birthday,
                self.transaction_context.config.sapling_activation_height(),
            );
            self.birthday
                .store(wallet_birthday, std::sync::atomic::Ordering::SeqCst);
        }
    }

    pub async fn add_imported_tk(&self, sk: String) -> String {
        if self.transaction_context.keys.read().await.encrypted {
            return "Error: Can't import transparent address key while wallet is encrypted"
                .to_string();
        }

        let sk = match TransparentKey::from_sk_string(&self.transaction_context.config, sk) {
            Err(e) => return format!("Error: {}", e),
            Ok(k) => k,
        };

        let address = sk.address.clone();

        if self
            .transaction_context
            .keys
            .read()
            .await
            .tkeys
            .iter()
            .find(|&tk| tk.address == address)
            .is_some()
        {
            return "Error: Key already exists".to_string();
        }

        self.transaction_context.keys.write().await.tkeys.push(sk);
        return address;
    }

    // Add a new imported spending key to the wallet
    /// NOTE: This will not rescan the wallet
    pub async fn add_imported_sapling_extsk(&self, sk: String, birthday: u64) -> String {
        self.add_imported_spend_key(
            &sk,
            self.transaction_context.config.hrp_sapling_private_key(),
            birthday,
            |k, hrp| decode_extended_spending_key(k, hrp).map(Some),
            Keys::zkeys,
            Keys::zkeys_mut,
            |wallet_key: &SaplingKey, new_key: &zcash_primitives::zip32::ExtendedSpendingKey| {
                wallet_key.extsk.is_some() && wallet_key.extsk.as_ref().unwrap() == &new_key.clone()
            },
            |wk, fvk| &wk.extfvk == fvk,
            SaplingKey::new_imported_sk,
            |key| {
                encode_payment_address(self.transaction_context.config.hrp_sapling_address(), &key)
            },
        )
        .await
    }

    // Add a new imported orchard secret key to the wallet
    /// NOTE: This will not rescan the wallet
    pub async fn add_imported_orchard_spending_key(&self, osk: String, birthday: u64) -> String {
        self.add_imported_spend_key(
            &osk,
            self.transaction_context
                .config
                .chain
                .hrp_orchard_spending_key(),
            birthday,
            decode_orchard_spending_key,
            Keys::okeys,
            Keys::okeys_mut,
            |wallet_key, new_key| {
                (&wallet_key.key)
                    .try_into()
                    .ok()
                    .map(|x: OrchardSpendingKey| x.to_bytes().to_vec())
                    == Some(new_key.to_bytes().to_vec())
            },
            |wk: &OrchardKey, fvk: &orchard::keys::FullViewingKey| {
                (&wk.key).try_into().ok() == Some(fvk.clone())
            },
            OrchardKey::new_imported_osk,
            |address: address::UnifiedAddress| {
                address.encode(&self.transaction_context.config.chain)
            },
        )
        .await
    }

    async fn add_imported_spend_key<
        WalletKey: self::traits::WalletKey + Clone,
        ViewKey: for<'a> From<&'a WalletKey::Sk>,
        DecodeError: std::fmt::Display,
    >(
        &self,
        key: &str,
        hrp: &str,
        birthday: u64,
        decoder: impl Fn(&str, &str) -> Result<Option<WalletKey::Sk>, DecodeError>,
        key_finder: impl Fn(&Keys) -> &Vec<WalletKey>,
        key_finder_mut: impl Fn(&mut Keys) -> &mut Vec<WalletKey>,
        key_matcher: impl Fn(&WalletKey, &WalletKey::Sk) -> bool,
        find_view_key: impl Fn(&WalletKey, &ViewKey) -> bool,
        key_importer: impl Fn(WalletKey::Sk) -> WalletKey,
        encode_address: impl Fn(WalletKey::Address) -> String,
    ) -> String {
        let address_getter = |decoded_key| {
            self.update_view_key(decoded_key, key_finder_mut, find_view_key, key_importer)
        };
        self.add_imported_key(
            key,
            hrp,
            birthday,
            decoder,
            key_finder,
            key_matcher,
            address_getter,
            encode_address,
        )
        .await
    }
    async fn add_imported_key<
        KeyType,
        WalletKey: self::traits::WalletKey + Clone,
        DecodeError: std::fmt::Display,
        Fut: Future<Output = WalletKey::Address>,
    >(
        &self,
        key: &str,
        hrp: &str,
        birthday: u64,
        decoder: impl Fn(&str, &str) -> Result<Option<KeyType>, DecodeError>,
        key_finder: impl Fn(&Keys) -> &Vec<WalletKey>,
        key_matcher: impl Fn(&WalletKey, &KeyType) -> bool,
        address_getter: impl FnOnce(KeyType) -> Fut,
        encode_address: impl Fn(WalletKey::Address) -> String,
    ) -> String {
        if self.transaction_context.keys.read().await.encrypted {
            return "Error: Can't import spending key while wallet is encrypted".to_string();
        }
        let decoded_key = match decoder(hrp, key) {
            Ok(Some(k)) => k,
            Ok(None) => {
                return format!(
                    "Error: Couldn't decode {} key",
                    std::any::type_name::<KeyType>()
                )
            }
            Err(e) => {
                return format!(
                    "Error importing {} key: {e}",
                    std::any::type_name::<KeyType>()
                )
            }
        };
        if key_finder(&*self.transaction_context.keys.read().await)
            .iter()
            .any(|k| key_matcher(k, &decoded_key))
        {
            return "Error: Key already exists".to_string();
        };
        // Adjust wallet birthday
        self.adjust_wallet_birthday(birthday);
        encode_address(address_getter(decoded_key).await)
    }
    async fn update_view_key<
        WalletKey: self::traits::WalletKey + Clone,
        ViewKey: for<'a> From<&'a WalletKey::Sk>,
    >(
        &self,
        decoded_key: WalletKey::Sk,
        key_finder_mut: impl Fn(&mut Keys) -> &mut Vec<WalletKey>,
        find_view_key: impl Fn(&WalletKey, &ViewKey) -> bool,
        key_importer: impl Fn(WalletKey::Sk) -> WalletKey,
    ) -> WalletKey::Address {
        let fvk = ViewKey::from(&decoded_key);
        let mut write_keys = self.transaction_context.keys.write().await;
        let write_keys = key_finder_mut(&mut *write_keys);
        let maybe_existing_key = write_keys.iter_mut().find(|k| find_view_key(k, &fvk));
        // If the viewing key exists, and is now being upgraded to the spending key, replace it in-place
        if maybe_existing_key.is_some() {
            let existing_key = maybe_existing_key.unwrap();
            existing_key.set_spend_key_for_view_key(decoded_key);
            existing_key.address()
        } else {
            let newkey = key_importer(decoded_key);
            write_keys.push(newkey.clone());
            newkey.address()
        }
    }
    // Add a new imported viewing key to the wallet
    /// NOTE: This will not rescan the wallet
    pub async fn add_imported_sapling_extfvk(&self, vk: String, birthday: u64) -> String {
        self.add_imported_key(
            &vk,
            self.transaction_context.config.hrp_sapling_viewing_key(),
            birthday,
            |k, hrp| decode_extended_full_viewing_key(k, hrp).map(Some),
            Keys::zkeys,
            |wallet_key, new_key| wallet_key.extfvk == new_key.clone(),
            |key| async {
                let newkey = SaplingKey::new_imported_viewkey(key);
                self.transaction_context
                    .keys
                    .write()
                    .await
                    .zkeys
                    .push(newkey.clone());
                newkey.zaddress
            },
            |address| {
                encode_payment_address(
                    self.transaction_context.config.hrp_sapling_address(),
                    &address,
                )
            },
        )
        .await
    }

    /// Clears all the downloaded blocks and resets the state back to the initial block.
    /// After this, the wallet's initial state will need to be set
    /// and the wallet will need to be rescanned
    pub async fn clear_all(&self) {
        self.blocks.write().await.clear();
        self.transaction_context
            .transaction_metadata_set
            .write()
            .await
            .clear();
    }

    pub async fn set_initial_block(&self, height: u64, hash: &str, _sapling_tree: &str) -> bool {
        let mut blocks = self.blocks.write().await;
        if !blocks.is_empty() {
            return false;
        }

        blocks.push(BlockData::new_with(height, hash));

        true
    }

    pub async fn last_scanned_height(&self) -> u64 {
        self.blocks
            .read()
            .await
            .first()
            .map(|block| block.height)
            .unwrap_or(self.transaction_context.config.sapling_activation_height() - 1)
    }

    pub async fn last_scanned_hash(&self) -> String {
        self.blocks
            .read()
            .await
            .first()
            .map(|block| block.hash())
            .unwrap_or_default()
    }

    async fn get_target_height(&self) -> Option<u32> {
        self.blocks
            .read()
            .await
            .first()
            .map(|block| block.height as u32 + 1)
    }

    /// Determines the target height for a transaction, and the offset from which to
    /// select anchors, based on the current synchronised block chain.
    async fn get_target_height_and_anchor_offset(&self) -> Option<(u32, usize)> {
        match {
            let blocks = self.blocks.read().await;
            (
                blocks.last().map(|block| block.height as u32),
                blocks.first().map(|block| block.height as u32),
            )
        } {
            (Some(min_height), Some(max_height)) => {
                let target_height = max_height + 1;

                // Select an anchor ANCHOR_OFFSET back from the target block,
                // unless that would be before the earliest block we have.
                let anchor_height = cmp::max(
                    target_height.saturating_sub(
                        *self
                            .transaction_context
                            .config
                            .anchor_offset
                            .last()
                            .unwrap(),
                    ),
                    min_height,
                );

                Some((target_height, (target_height - anchor_height) as usize))
            }
            _ => None,
        }
    }

    /// Get the height of the anchor block
    pub async fn get_anchor_height(&self) -> u32 {
        match self.get_target_height_and_anchor_offset().await {
            Some((height, anchor_offset)) => height - anchor_offset as u32 - 1,
            None => return 0,
        }
    }

    pub fn memo_str(memo: Option<Memo>) -> Option<String> {
        match memo {
            Some(Memo::Text(m)) => Some(m.to_string()),
            _ => None,
        }
    }

    pub async fn maybe_verified_sapling_balance(&self, addr: Option<String>) -> u64 {
        self.shielded_balance::<SaplingNoteAndMetadata>(addr, &[])
            .await
    }

    pub async fn maybe_verified_orchard_balance(&self, addr: Option<String>) -> u64 {
        self.shielded_balance::<OrchardNoteAndMetadata>(addr, &[])
            .await
    }

    async fn shielded_balance<NnMd>(
        &self,
        target_addr: Option<String>,
        filters: &[Box<dyn Fn(&&NnMd, &TransactionMetadata) -> bool + '_>],
    ) -> u64
    where
        NnMd: traits::NoteAndMetadata,
    {
        let filter_notes_by_target_addr = |notedata: &&NnMd| match target_addr.as_ref() {
            Some(addr) => {
                use self::traits::Recipient as _;
                let diversified_address = &notedata
                    .fvk()
                    .diversified_address(*notedata.diversifier())
                    .unwrap();
                *addr
                    == diversified_address
                        .b32encode_for_network(&self.transaction_context.config.chain)
            }
            None => true, // If the addr is none, then get all addrs.
        };
        self.transaction_context
            .transaction_metadata_set
            .read()
            .await
            .current
            .values()
            .map(|transaction| {
                let mut filtered_notes: Box<dyn Iterator<Item = &NnMd>> = Box::new(
                    NnMd::transaction_metadata_notes(transaction)
                        .iter()
                        .filter(filter_notes_by_target_addr),
                );
                // All filters in iterator are applied, by this loop
                for filtering_fn in filters {
                    filtered_notes =
                        Box::new(filtered_notes.filter(|nnmd| filtering_fn(nnmd, transaction)))
                }
                filtered_notes
                    .map(|notedata| {
                        if notedata.spent().is_none() && notedata.unconfirmed_spent().is_none() {
                            <NnMd as traits::NoteAndMetadata>::value(notedata)
                        } else {
                            0
                        }
                    })
                    .sum::<u64>()
            })
            .sum::<u64>()
    }

    // Get all (unspent) utxos. Unconfirmed spent utxos are included
    pub async fn get_utxos(&self) -> Vec<Utxo> {
        self.transaction_context
            .transaction_metadata_set
            .read()
            .await
            .current
            .values()
            .flat_map(|transaction| transaction.utxos.iter().filter(|utxo| utxo.spent.is_none()))
            .map(|utxo| utxo.clone())
            .collect::<Vec<Utxo>>()
    }

    pub async fn tbalance(&self, addr: Option<String>) -> u64 {
        self.get_utxos()
            .await
            .iter()
            .filter(|utxo| match addr.as_ref() {
                Some(a) => utxo.address == *a,
                None => true,
            })
            .map(|utxo| utxo.value)
            .sum::<u64>()
    }

    /// The following functions use a filter/map functional approach to
    /// expressively unpack different kinds of transaction data.
    pub async fn unverified_sapling_balance(&self, target_addr: Option<String>) -> u64 {
        let anchor_height = self.get_anchor_height().await;

        let keys = self.transaction_context.keys.read().await;

        let filters: &[Box<dyn Fn(&&SaplingNoteAndMetadata, &TransactionMetadata) -> bool>] = &[
            Box::new(|notedata: &&SaplingNoteAndMetadata, _| {
                // Check to see if we have this note's spending key.
                keys.have_sapling_spending_key(&notedata.extfvk)
            }),
            Box::new(|_, transaction: &TransactionMetadata| {
                transaction.block > BlockHeight::from_u32(anchor_height)
            }),
        ];
        self.shielded_balance(target_addr, filters).await
    }

    pub async fn unverified_orchard_balance(&self, target_addr: Option<String>) -> u64 {
        let anchor_height = self.get_anchor_height().await;

        let keys = self.transaction_context.keys.read().await;

        let filters: &[Box<dyn Fn(&&OrchardNoteAndMetadata, &TransactionMetadata) -> bool>] = &[
            Box::new(|notedata, _| {
                // Check to see if we have this note's spending key.
                keys.have_orchard_spending_key(&notedata.fvk.to_ivk(orchard::keys::Scope::External))
            }),
            Box::new(|_, transaction: &TransactionMetadata| {
                transaction.block > BlockHeight::from_u32(anchor_height)
            }),
        ];
        self.shielded_balance(target_addr, filters).await
    }

    pub async fn verified_sapling_balance(&self, target_addr: Option<String>) -> u64 {
        self.verified_balance::<SaplingNoteAndMetadata>(target_addr)
            .await
    }

    pub async fn verified_orchard_balance(&self, target_addr: Option<String>) -> u64 {
        self.verified_balance::<OrchardNoteAndMetadata>(target_addr)
            .await
    }

    async fn verified_balance<NnMd: NoteAndMetadata>(&self, target_addr: Option<String>) -> u64 {
        let anchor_height = self.get_anchor_height().await;
        let filters: &[Box<dyn Fn(&&NnMd, &TransactionMetadata) -> bool>] =
            &[Box::new(|_, transaction| {
                transaction.block <= BlockHeight::from_u32(anchor_height)
            })];
        self.shielded_balance::<NnMd>(target_addr, filters).await
    }

    pub async fn spendable_sapling_balance(&self, target_addr: Option<String>) -> u64 {
        let anchor_height = self.get_anchor_height().await;
        let keys = self.transaction_context.keys.read().await;
        let filters: &[Box<dyn Fn(&&SaplingNoteAndMetadata, &TransactionMetadata) -> bool>] = &[
            Box::new(|_, transaction| transaction.block <= BlockHeight::from_u32(anchor_height)),
            Box::new(|nnmd, _| {
                keys.have_sapling_spending_key(&nnmd.extfvk) && nnmd.witnesses.len() > 0
            }),
        ];
        self.shielded_balance(target_addr, filters).await
    }

    pub async fn spendable_orchard_balance(&self, target_addr: Option<String>) -> u64 {
        let anchor_height = self.get_anchor_height().await;
        let keys = self.transaction_context.keys.read().await;
        let filters: &[Box<dyn Fn(&&OrchardNoteAndMetadata, &TransactionMetadata) -> bool>] = &[
            Box::new(|_, transaction| transaction.block <= BlockHeight::from_u32(anchor_height)),
            Box::new(|nnmd, _| {
                keys.have_orchard_spending_key(&nnmd.fvk.to_ivk(orchard::keys::Scope::External))
                    && nnmd.witnesses.len() > 0
            }),
        ];
        self.shielded_balance(target_addr, filters).await
    }

    pub async fn remove_unused_taddrs(&self) {
        let taddrs = self.transaction_context.keys.read().await.get_all_taddrs();
        if taddrs.len() <= 1 {
            return;
        }

        let highest_account = self
            .transaction_context
            .transaction_metadata_set
            .read()
            .await
            .current
            .values()
            .flat_map(|wtx| {
                wtx.utxos.iter().map(|u| {
                    taddrs
                        .iter()
                        .position(|taddr| *taddr == u.address)
                        .unwrap_or(taddrs.len())
                })
            })
            .max();

        if highest_account.is_none() {
            return;
        }

        if highest_account.unwrap() == 0 {
            // Remove unused addresses
            self.transaction_context
                .keys
                .write()
                .await
                .tkeys
                .truncate(1);
        }
    }

    pub async fn remove_unused_zaddrs(&self) {
        let zaddrs = self
            .transaction_context
            .keys
            .read()
            .await
            .get_all_sapling_addresses();
        if zaddrs.len() <= 1 {
            return;
        }

        let highest_account = self
            .transaction_context
            .transaction_metadata_set
            .read()
            .await
            .current
            .values()
            .flat_map(|wtx| {
                wtx.sapling_notes.iter().map(|n| {
                    let (_, pa) = n.extfvk.default_address();
                    let zaddr = encode_payment_address(
                        self.transaction_context.config.hrp_sapling_address(),
                        &pa,
                    );
                    zaddrs
                        .iter()
                        .position(|za| *za == zaddr)
                        .unwrap_or(zaddrs.len())
                })
            })
            .max();

        if highest_account.is_none() {
            return;
        }

        if highest_account.unwrap() == 0 {
            // Remove unused addresses
            self.transaction_context
                .keys
                .write()
                .await
                .zkeys
                .truncate(1);
        }
    }

    pub async fn decrypt_message(&self, enc: Vec<u8>) -> Option<Message> {
        // Collect all the ivks in the wallet
        let ivks: Vec<_> = self
            .transaction_context
            .keys
            .read()
            .await
            .get_all_sapling_extfvks()
            .iter()
            .map(|extfvk| extfvk.fvk.vk.ivk())
            .collect();

        // Attempt decryption with all available ivks, one at a time. This is pretty fast, so need need for fancy multithreading
        for ivk in ivks {
            if let Ok(msg) = Message::decrypt(&enc, &ivk) {
                // If decryption succeeded for this IVK, return the decrypted memo and the matched address
                return Some(msg);
            }
        }

        // If nothing matched
        None
    }

    // Add the spent_at_height for each sapling note that has been spent. This field was added in wallet version 8,
    // so for older wallets, it will need to be added
    pub async fn fix_spent_at_height(&self) {
        // First, build an index of all the transaction_ids and the heights at which they were spent.
        let spent_transaction_id_map: HashMap<_, _> = self
            .transaction_context
            .transaction_metadata_set
            .read()
            .await
            .current
            .iter()
            .map(|(transaction_id, wtx)| (transaction_id.clone(), wtx.block))
            .collect();

        // Go over all the sapling notes that might need updating
        self.transaction_context
            .transaction_metadata_set
            .write()
            .await
            .current
            .values_mut()
            .for_each(|wtx| {
                wtx.sapling_notes
                    .iter_mut()
                    .filter(|nd| nd.spent.is_some() && nd.spent.unwrap().1 == 0)
                    .for_each(|nd| {
                        let transaction_id = nd.spent.unwrap().0;
                        if let Some(height) =
                            spent_transaction_id_map.get(&transaction_id).map(|b| *b)
                        {
                            nd.spent = Some((transaction_id, height.into()));
                        }
                    })
            });

        // Go over all the Utxos that might need updating
        self.transaction_context
            .transaction_metadata_set
            .write()
            .await
            .current
            .values_mut()
            .for_each(|wtx| {
                wtx.utxos
                    .iter_mut()
                    .filter(|utxo| utxo.spent.is_some() && utxo.spent_at_height.is_none())
                    .for_each(|utxo| {
                        utxo.spent_at_height = spent_transaction_id_map
                            .get(&utxo.spent.unwrap())
                            .map(|b| u32::from(*b) as i32);
                    })
            });
    }

    async fn select_notes_and_utxos(
        &self,
        target_amount: Amount,
        transparent_only: bool,
        shield_transparenent: bool,
        prefer_orchard_over_sapling: bool,
    ) -> (
        Vec<SpendableOrchardNote>,
        Vec<SpendableSaplingNote>,
        Vec<Utxo>,
        Amount,
    ) {
        // First, if we are allowed to pick transparent value, pick them all
        let utxos = if transparent_only || shield_transparenent {
            self.get_utxos()
                .await
                .iter()
                .filter(|utxo| utxo.unconfirmed_spent.is_none() && utxo.spent.is_none())
                .map(|utxo| utxo.clone())
                .collect::<Vec<_>>()
        } else {
            vec![]
        };

        // Check how much we've selected
        let transparent_value_selected = utxos.iter().fold(Amount::zero(), |prev, utxo| {
            (prev + Amount::from_u64(utxo.value).unwrap()).unwrap()
        });

        // If we are allowed only transparent funds or we've selected enough then return
        if transparent_only || transparent_value_selected >= target_amount {
            return (vec![], vec![], utxos, transparent_value_selected);
        }

        let mut sapling_value_selected = Amount::zero();
        let mut sapling_notes = vec![];
        // Select the minimum number of notes required to satisfy the target value
        if prefer_orchard_over_sapling {
            let sapling_candidates = self
                .get_all_domain_specific_notes::<SaplingDomain<zingoconfig::Network>>()
                .await;
            (sapling_notes, sapling_value_selected) =
                Self::add_notes_to_total::<SaplingDomain<zingoconfig::Network>>(
                    sapling_candidates,
                    (target_amount - transparent_value_selected).unwrap(),
                );
            if transparent_value_selected + sapling_value_selected >= Some(target_amount) {
                return (
                    vec![],
                    sapling_notes,
                    utxos,
                    (transparent_value_selected + sapling_value_selected).unwrap(),
                );
            }
        }
        let orchard_candidates = self.get_all_domain_specific_notes::<OrchardDomain>().await;
        let (orchard_notes, orchard_value_selected) = Self::add_notes_to_total::<OrchardDomain>(
            orchard_candidates,
            (target_amount - transparent_value_selected - sapling_value_selected).unwrap(),
        );
        if transparent_value_selected + sapling_value_selected + orchard_value_selected
            >= Some(target_amount)
        {
            return (
                orchard_notes,
                sapling_notes,
                utxos,
                (transparent_value_selected + sapling_value_selected + orchard_value_selected)
                    .unwrap(),
            );
        }
        if !prefer_orchard_over_sapling {
            let sapling_candidates = self
                .get_all_domain_specific_notes::<SaplingDomain<zingoconfig::Network>>()
                .await;
            (sapling_notes, sapling_value_selected) =
                Self::add_notes_to_total::<SaplingDomain<zingoconfig::Network>>(
                    sapling_candidates,
                    (target_amount - transparent_value_selected).unwrap(),
                );
            if transparent_value_selected + sapling_value_selected + orchard_value_selected
                >= Some(target_amount)
            {
                return (
                    orchard_notes,
                    sapling_notes,
                    utxos,
                    (transparent_value_selected + sapling_value_selected + orchard_value_selected)
                        .unwrap(),
                );
            }
        }

        // If we can't select enough, then we need to return empty handed
        (vec![], vec![], vec![], Amount::zero())
    }

    async fn get_all_domain_specific_notes<D>(&self) -> Vec<Vec<D::SpendableNote>>
    where
        D: DomainWalletExt<zingoconfig::Network>,
        <D as Domain>::Recipient: traits::Recipient,
        <D as Domain>::Note: PartialEq + Clone,
    {
        let keys_arc = self.keys();
        let keys = keys_arc.read().await;
        let notes_arc = self.transactions();
        let notes = notes_arc.read().await;
        self.transaction_context
            .config
            .anchor_offset
            .iter()
            .map(|anchor_offset| {
                let mut candidate_notes = notes
                    .current
                    .iter()
                    .flat_map(|(transaction_id, transaction)| {
                        D::WalletNote::transaction_metadata_notes(transaction)
                            .iter()
                            .map(move |note| (*transaction_id, note))
                    })
                    .filter(|(_, note)| note.value() > 0)
                    .filter_map(|(transaction_id, note)| {
                        // Filter out notes that are already spent
                        if note.spent().is_some() || note.unconfirmed_spent().is_some() {
                            None
                        } else {
                            // Get the spending key for the selected fvk, if we have it
                            let extsk = keys.get_sk_for_fvk::<D>(&note.fvk());
                            SpendableNote::from(
                                transaction_id,
                                note,
                                *anchor_offset as usize,
                                &extsk,
                            )
                        }
                    })
                    .collect::<Vec<D::SpendableNote>>();
                candidate_notes.sort_by_key(|spendable_note| {
                    D::WalletNote::value_from_note(&spendable_note.note())
                });
                candidate_notes
            })
            .collect()
    }

    fn add_notes_to_total<D: DomainWalletExt<zingoconfig::Network>>(
        candidates: Vec<Vec<D::SpendableNote>>,
        target_amount: Amount,
    ) -> (Vec<D::SpendableNote>, Amount)
    where
        D::Note: PartialEq + Clone,
        D::Recipient: traits::Recipient,
    {
        let mut notes = vec![];
        let mut value_selected = Amount::zero();
        let mut candidates = candidates.into_iter();
        loop {
            if let Some(candidate_set) = candidates.next() {
                notes = candidate_set
                    .into_iter()
                    .scan(Amount::zero(), |running_total, spendable| {
                        if *running_total >= target_amount {
                            None
                        } else {
                            *running_total +=
                                Amount::from_u64(D::WalletNote::value_from_note(&spendable.note()))
                                    .unwrap();
                            Some(spendable)
                        }
                    })
                    .collect::<Vec<_>>();
                value_selected = notes.iter().fold(Amount::zero(), |prev, sn| {
                    (prev + Amount::from_u64(D::WalletNote::value_from_note(&sn.note())).unwrap())
                        .unwrap()
                });

                if value_selected >= target_amount {
                    break (notes, value_selected);
                }
            } else {
                break (notes, value_selected);
            }
        }
    }

    pub async fn send_to_address<F, Fut, P: TxProver>(
        &self,
        prover: P,
        transparent_only: bool,
        migrate_sapling_to_orchard: bool,
        tos: Vec<(&str, u64, Option<String>)>,
        broadcast_fn: F,
    ) -> Result<(String, Vec<u8>), String>
    where
        F: Fn(Box<[u8]>) -> Fut,
        Fut: Future<Output = Result<String, String>>,
    {
        // Reset the progress to start. Any errors will get recorded here
        self.reset_send_progress().await;

        // Call the internal function
        match self
            .send_to_address_internal(
                prover,
                transparent_only,
                migrate_sapling_to_orchard,
                tos,
                broadcast_fn,
            )
            .await
        {
            Ok((transaction_id, raw_transaction)) => {
                self.set_send_success(transaction_id.clone()).await;
                Ok((transaction_id, raw_transaction))
            }
            Err(e) => {
                self.set_send_error(format!("{}", e)).await;
                Err(e)
            }
        }
    }

    async fn send_to_address_internal<F, Fut, P: TxProver>(
        &self,
        prover: P,
        transparent_only: bool,
        migrate_sapling_to_orchard: bool,
        tos: Vec<(&str, u64, Option<String>)>,
        broadcast_fn: F,
    ) -> Result<(String, Vec<u8>), String>
    where
        F: Fn(Box<[u8]>) -> Fut,
        Fut: Future<Output = Result<String, String>>,
    {
        if !self.transaction_context.keys.read().await.unlocked {
            return Err("Cannot spend while wallet is locked".to_string());
        }

        let start_time = now();
        if tos.len() == 0 {
            return Err("Need at least one destination address".to_string());
        }

        let total_value = tos.iter().map(|to| to.1).sum::<u64>();
        println!(
            "0: Creating transaction sending {} ztoshis to {} addresses",
            total_value,
            tos.len()
        );

        // Convert address (str) to RecepientAddress and value to Amount
        let recepients = tos
            .iter()
            .map(|to| {
                let ra = match address::RecipientAddress::decode(
                    &self.transaction_context.config.chain,
                    to.0,
                ) {
                    Some(to) => to,
                    None => {
                        let e = format!("Invalid recipient address: '{}'", to.0);
                        error!("{}", e);
                        return Err(e);
                    }
                };

                let value = Amount::from_u64(to.1).unwrap();

                Ok((ra, value, to.2.clone()))
            })
            .collect::<Result<Vec<(address::RecipientAddress, Amount, Option<String>)>, String>>(
            )?;

        // Select notes to cover the target value
        println!("{}: Selecting notes", now() - start_time);

        let target_amount = (Amount::from_u64(total_value).unwrap() + DEFAULT_FEE).unwrap();
        let target_height = match self.get_target_height().await {
            Some(h) => BlockHeight::from_u32(h),
            None => return Err("No blocks in wallet to target, please sync first".to_string()),
        };

        // Create a map from address -> sk for all taddrs, so we can spend from the
        // right address
        let address_to_sk = self
            .transaction_context
            .keys
            .read()
            .await
            .get_taddr_to_sk_map();

        let (orchard_notes, sapling_notes, utxos, selected_value) = self
            .select_notes_and_utxos(
                target_amount,
                transparent_only,
                true,
                migrate_sapling_to_orchard,
            )
            .await;
        if selected_value < target_amount {
            let e = format!(
                "Insufficient verified funds. Have {} zats, need {} zats. NOTE: funds need at least {} confirmations before they can be spent.",
                u64::from(selected_value), u64::from(target_amount), self.transaction_context.config
                .anchor_offset.last().unwrap() + 1
            );
            error!("{}", e);
            return Err(e);
        }
        println!("Selected notes worth {}", u64::from(selected_value));

        let orchard_anchor = if let Some(note) = orchard_notes.get(0) {
            note.witness.root()
        } else {
            if let Some(tree_state) = &*self.verified_tree.read().await {
                let ref orchard_tree = tree_state.orchard_tree;
                CommitmentTree::read(hex::decode(orchard_tree).unwrap().as_slice())
                    .unwrap()
                    .root()
            } else {
                return Err("No last known verified tree".to_string());
            }
        };
        let mut builder = Builder::with_orchard_anchor(
            self.transaction_context.config.chain,
            target_height,
            orchard::Anchor::from(orchard_anchor),
        );
        println!(
            "{}: Adding {} sapling notes, {} orchard notes, and {} utxos",
            now() - start_time,
            sapling_notes.len(),
            orchard_notes.len(),
            utxos.len()
        );

        // Add all tinputs
        utxos
            .iter()
            .map(|utxo| {
                let outpoint: OutPoint = utxo.to_outpoint();

                let coin = TxOut {
                    value: Amount::from_u64(utxo.value).unwrap(),
                    script_pubkey: Script { 0: utxo.script.clone() },
                };

                match address_to_sk.get(&utxo.address) {
                    Some(sk) => builder.add_transparent_input(*sk, outpoint.clone(), coin.clone()),
                    None => {
                        // Something is very wrong
                        let e = format!("Couldn't find the secreykey for taddr {}", utxo.address);
                        error!("{}", e);

                        Err(zcash_primitives::transaction::builder::Error::TransparentBuild(
                            zcash_primitives::transaction::components::transparent::builder::Error::InvalidAddress,
                        ))
                    }
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("{:?}", e))?;

        for selected in sapling_notes.iter() {
            println!("Adding sapling spend");
            if let Err(e) = builder.add_sapling_spend(
                selected.extsk.clone(),
                selected.diversifier,
                selected.note.clone(),
                selected.witness.path().unwrap(),
            ) {
                let e = format!("Error adding note: {:?}", e);
                error!("{}", e);
                return Err(e);
            }
        }

        for selected in orchard_notes.iter() {
            println!("Adding orchard spend");
            let path = selected.witness.path().unwrap();
            if let Err(e) = builder.add_orchard_spend(
                selected.sk.clone(),
                selected.note.clone(),
                orchard::tree::MerklePath::from((
                    incrementalmerkletree::Position::from(path.position as usize),
                    path.auth_path
                        .iter()
                        .map(|(node, _)| node.clone())
                        .collect(),
                )),
            ) {
                let e = format!("Error adding note: {:?}", e);
                error!("{}", e);
                return Err(e);
            }
        }

        //TODO: Send change to orchard instead of sapling
        // If no Sapling notes were added, add the change address manually. That is,
        // send the change to our sapling address manually. Note that if a sapling note was spent,
        // the builder will automatically send change to that address
        if sapling_notes.len() == 0 {
            builder.send_change_to(
                self.keys().read().await.zkeys[0].extfvk.fvk.ovk,
                self.keys().read().await.zkeys[0].zaddress.clone(),
            );
        }

        // We'll use the first ovk to encrypt outgoing transactions
        let sapling_ovk = self.keys().read().await.zkeys[0].extfvk.fvk.ovk;
        let orchard_ovk = self
            .keys()
            .read()
            .await
            .okeys
            .get(0)
            .and_then(OrchardKey::ovk);

        let mut total_z_recepients = 0u32;
        for (to, value, memo) in recepients {
            // Compute memo if it exists
            let encoded_memo = match memo {
                None => MemoBytes::from(Memo::Empty),
                Some(s) => {
                    // If the string starts with an "0x", and contains only hex chars ([a-f0-9]+) then
                    // interpret it as a hex
                    match utils::interpret_memo_string(s) {
                        Ok(m) => m,
                        Err(e) => {
                            error!("{}", e);
                            return Err(e);
                        }
                    }
                }
            };

            println!("{}: Adding output", now() - start_time);

            if let Err(e) = match to {
                address::RecipientAddress::Shielded(to) => {
                    total_z_recepients += 1;
                    builder.add_sapling_output(Some(sapling_ovk), to.clone(), value, encoded_memo)
                }
                address::RecipientAddress::Transparent(to) => {
                    builder.add_transparent_output(&to, value)
                }
                address::RecipientAddress::Unified(ua) => {
                    if let Some(orchard_addr) = ua.orchard() {
                        builder.add_orchard_output(
                            orchard_ovk.clone(),
                            orchard_addr.clone(),
                            u64::from(value),
                            encoded_memo,
                        )
                    } else if let Some(sapling_addr) = ua.sapling() {
                        total_z_recepients += 1;
                        builder.add_sapling_output(
                            Some(sapling_ovk),
                            sapling_addr.clone(),
                            value,
                            encoded_memo,
                        )
                    } else {
                        return Err("Received UA with no Orchard or Sapling receiver".to_string());
                    }
                }
            } {
                let e = format!("Error adding output: {:?}", e);
                error!("{}", e);
                return Err(e);
            }
        }

        // Set up a channel to recieve updates on the progress of building the transaction.
        let (transmitter, receiver) = channel::<Progress>();
        let progress = self.send_progress.clone();

        // Use a separate thread to handle sending from std::mpsc to tokio::sync::mpsc
        let (transmitter2, mut receiver2) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            while let Ok(r) = receiver.recv() {
                transmitter2.send(r.cur()).unwrap();
            }
        });

        let progress_handle = tokio::spawn(async move {
            while let Some(r) = receiver2.recv().await {
                println!("Progress: {}", r);
                progress.write().await.progress = r;
            }

            progress.write().await.is_send_in_progress = false;
        });

        {
            let mut p = self.send_progress.write().await;
            p.is_send_in_progress = true;
            p.progress = 0;
            p.total = sapling_notes.len() as u32 + total_z_recepients;
        }

        println!("{}: Building transaction", now() - start_time);

        builder.with_progress_notifier(transmitter);
        let (transaction, _) = match builder.build(&prover) {
            Ok(res) => res,
            Err(e) => {
                let e = format!("Error creating transaction: {:?}", e);
                error!("{}", e);
                self.send_progress.write().await.is_send_in_progress = false;
                return Err(e);
            }
        };

        // Wait for all the progress to be updated
        progress_handle.await.unwrap();

        println!("{}: Transaction created", now() - start_time);
        println!("Transaction ID: {}", transaction.txid());

        {
            self.send_progress.write().await.is_send_in_progress = false;
        }

        // Create the transaction bytes
        let mut raw_transaction = vec![];
        transaction.write(&mut raw_transaction).unwrap();

        let transaction_id = broadcast_fn(raw_transaction.clone().into_boxed_slice()).await?;

        // Mark notes as spent.
        {
            // Mark sapling notes as unconfirmed spent
            let mut transactions = self
                .transaction_context
                .transaction_metadata_set
                .write()
                .await;
            for selected in sapling_notes {
                let mut spent_note = transactions
                    .current
                    .get_mut(&selected.transaction_id)
                    .unwrap()
                    .sapling_notes
                    .iter_mut()
                    .find(|nd| nd.nullifier == selected.nullifier)
                    .unwrap();
                spent_note.unconfirmed_spent = Some((transaction.txid(), u32::from(target_height)));
            }
            // Mark orchard notes as unconfirmed spent
            for selected in orchard_notes {
                let mut spent_note = transactions
                    .current
                    .get_mut(&selected.transaction_id)
                    .unwrap()
                    .orchard_notes
                    .iter_mut()
                    .find(|nd| nd.nullifier == selected.nullifier)
                    .unwrap();
                spent_note.unconfirmed_spent = Some((transaction.txid(), u32::from(target_height)));
            }

            // Mark this utxo as unconfirmed spent
            for utxo in utxos {
                let mut spent_utxo = transactions
                    .current
                    .get_mut(&utxo.txid)
                    .unwrap()
                    .utxos
                    .iter_mut()
                    .find(|u| utxo.txid == u.txid && utxo.output_index == u.output_index)
                    .unwrap();
                spent_utxo.unconfirmed_spent = Some((transaction.txid(), u32::from(target_height)));
            }
        }

        // Add this transaction to the mempool structure
        {
            let price = self.price.read().await.clone();

            self.transaction_context
                .scan_full_tx(
                    transaction,
                    target_height.into(),
                    true,
                    now() as u32,
                    TransactionMetadata::get_price(now(), &price),
                )
                .await;
        }

        Ok((transaction_id, raw_transaction))
    }

    pub async fn encrypt(&self, passwd: String) -> io::Result<()> {
        self.transaction_context.keys.write().await.encrypt(passwd)
    }

    pub async fn lock(&self) -> io::Result<()> {
        self.transaction_context.keys.write().await.lock()
    }

    pub async fn unlock(&self, passwd: String) -> io::Result<()> {
        self.transaction_context.keys.write().await.unlock(passwd)
    }

    pub async fn remove_encryption(&self, passwd: String) -> io::Result<()> {
        self.transaction_context
            .keys
            .write()
            .await
            .remove_encryption(passwd)
    }
}
fn decode_orchard_spending_key(
    expected_hrp: &str,
    s: &str,
) -> Result<Option<OrchardSpendingKey>, String> {
    match bech32::decode(&s) {
        Ok((hrp, bytes, variant)) => {
            use bech32::FromBase32;
            if hrp != expected_hrp {
                return Err(format!(
                    "invalid human-readable-part {hrp}, expected {expected_hrp}.",
                ));
            }
            if variant != bech32::Variant::Bech32m {
                return Err("Wrong encoding, expected bech32m".to_string());
            }
            match Vec::<u8>::from_base32(&bytes).map(<[u8; 32]>::try_from) {
                Ok(Ok(b)) => Ok(OrchardSpendingKey::from_bytes(b).into()),
                Ok(Err(e)) => Err(format!("key {s} decodes to {e:?}, which is not 32 bytes")),
                Err(e) => Err(e.to_string()),
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod test {
    use zcash_primitives::transaction::components::Amount;

    use crate::{
        apply_scenario,
        blaze::test_utils::{incw_to_string, FakeTransaction},
        lightclient::test_server::{
            clean_shutdown, mine_numblocks_each_with_two_sap_txs, mine_pending_blocks,
            NBlockFCBLScenario,
        },
    };

    mod bench_select_notes_and_utxos {
        use super::*;
        crate::apply_scenario! {insufficient_funds_0_present_needed_1 10}
        async fn insufficient_funds_0_present_needed_1(scenario: NBlockFCBLScenario) {
            let NBlockFCBLScenario { lightclient, .. } = scenario;
            let sufficient_funds = lightclient
                .wallet
                .select_notes_and_utxos(Amount::from_u64(1).unwrap(), false, false, false)
                .await;
            assert_eq!(Amount::from_u64(0).unwrap(), sufficient_funds.3);
        }

        crate::apply_scenario! {insufficient_funds_1_present_needed_1 10}
        async fn insufficient_funds_1_present_needed_1(scenario: NBlockFCBLScenario) {
            let NBlockFCBLScenario {
                lightclient,
                data,
                mut fake_compactblock_list,
                ..
            } = scenario;
            let extended_fvk = lightclient
                .wallet
                .keys()
                .read()
                .await
                .get_all_sapling_extfvks()[0]
                .clone();
            let (_, _, _) = fake_compactblock_list.create_coinbase_transaction(&extended_fvk, 1);
            mine_pending_blocks(&mut fake_compactblock_list, &data, &lightclient).await;
            assert_eq!(
                lightclient
                    .wallet
                    .maybe_verified_sapling_balance(None)
                    .await,
                1
            );
            let sufficient_funds = lightclient
                .wallet
                .select_notes_and_utxos(Amount::from_u64(1).unwrap(), false, false, false)
                .await;
            assert_eq!(Amount::from_u64(0).unwrap(), sufficient_funds.3);
        }
        crate::apply_scenario! {insufficient_funds_1_plus_txfee_present_needed_1 10}
        async fn insufficient_funds_1_plus_txfee_present_needed_1(scenario: NBlockFCBLScenario) {
            let NBlockFCBLScenario {
                lightclient,
                data,
                mut fake_compactblock_list,
                ..
            } = scenario;
            let extended_fvk = lightclient
                .wallet
                .keys()
                .read()
                .await
                .get_all_sapling_extfvks()[0]
                .clone();
            use zcash_primitives::transaction::components::amount::DEFAULT_FEE;
            let (_, _, _) = fake_compactblock_list
                .create_coinbase_transaction(&extended_fvk, 1 + u64::from(DEFAULT_FEE));
            for _ in 0..=3 {
                fake_compactblock_list.add_empty_block();
            }
            mine_pending_blocks(&mut fake_compactblock_list, &data, &lightclient).await;
            assert_eq!(
                lightclient
                    .wallet
                    .maybe_verified_sapling_balance(None)
                    .await,
                1_001
            );
            let sufficient_funds = lightclient
                .wallet
                .select_notes_and_utxos(Amount::from_u64(1).unwrap(), false, false, false)
                .await;
            assert_eq!(Amount::from_u64(1_001).unwrap(), sufficient_funds.3);
        }
    }
    apply_scenario! {z_t_note_selection 10}
    async fn z_t_note_selection(scenario: NBlockFCBLScenario) {
        let NBlockFCBLScenario {
            data,
            mut lightclient,
            mut fake_compactblock_list,
            ..
        } = scenario;
        // 2. Send an incoming transaction to fill the wallet
        let extfvk1 = lightclient
            .wallet
            .keys()
            .read()
            .await
            .get_all_sapling_extfvks()[0]
            .clone();
        let value = 100_000;
        let (transaction, _height, _) =
            fake_compactblock_list.create_coinbase_transaction(&extfvk1, value);
        let txid = transaction.txid();
        mine_pending_blocks(&mut fake_compactblock_list, &data, &lightclient).await;

        assert_eq!(lightclient.wallet.last_scanned_height().await, 11);

        // 3. With one confirmation, we should be able to select the note
        let amt = Amount::from_u64(10_000).unwrap();
        // Reset the anchor offsets
        lightclient.wallet.transaction_context.config.anchor_offset = [9, 4, 2, 1, 0];
        let (_orchard_notes, sapling_notes, utxos, selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, false, false)
            .await;
        assert!(selected >= amt);
        assert_eq!(sapling_notes.len(), 1);
        assert_eq!(sapling_notes[0].note.value, value);
        assert_eq!(utxos.len(), 0);
        assert_eq!(
            incw_to_string(&sapling_notes[0].witness),
            incw_to_string(
                lightclient
                    .wallet
                    .transaction_context
                    .transaction_metadata_set
                    .read()
                    .await
                    .current
                    .get(&txid)
                    .unwrap()
                    .sapling_notes[0]
                    .witnesses
                    .last()
                    .unwrap()
            )
        );

        // With min anchor_offset at 1, we can't select any notes
        lightclient.wallet.transaction_context.config.anchor_offset = [9, 4, 2, 1, 1];
        let (_orchard_notes, sapling_notes, utxos, _selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, false, false)
            .await;
        assert_eq!(sapling_notes.len(), 0);
        assert_eq!(utxos.len(), 0);

        // Mine 1 block, then it should be selectable
        mine_numblocks_each_with_two_sap_txs(&mut fake_compactblock_list, &data, &lightclient, 1)
            .await;

        let (_orchard_notes, sapling_notes, utxos, selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, false, false)
            .await;
        assert!(selected >= amt);
        assert_eq!(sapling_notes.len(), 1);
        assert_eq!(sapling_notes[0].note.value, value);
        assert_eq!(utxos.len(), 0);
        assert_eq!(
            incw_to_string(&sapling_notes[0].witness),
            incw_to_string(
                lightclient
                    .wallet
                    .transaction_context
                    .transaction_metadata_set
                    .read()
                    .await
                    .current
                    .get(&txid)
                    .unwrap()
                    .sapling_notes[0]
                    .witnesses
                    .get_from_last(1)
                    .unwrap()
            )
        );

        // Mine 15 blocks, then selecting the note should result in witness only 10 blocks deep
        mine_numblocks_each_with_two_sap_txs(&mut fake_compactblock_list, &data, &lightclient, 15)
            .await;
        lightclient.wallet.transaction_context.config.anchor_offset = [9, 4, 2, 1, 1];
        let (_orchard_notes, sapling_notes, utxos, selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, true, false)
            .await;
        assert!(selected >= amt);
        assert_eq!(sapling_notes.len(), 1);
        assert_eq!(sapling_notes[0].note.value, value);
        assert_eq!(utxos.len(), 0);
        assert_eq!(
            incw_to_string(&sapling_notes[0].witness),
            incw_to_string(
                lightclient
                    .wallet
                    .transaction_context
                    .transaction_metadata_set
                    .read()
                    .await
                    .current
                    .get(&txid)
                    .unwrap()
                    .sapling_notes[0]
                    .witnesses
                    .get_from_last(9)
                    .unwrap()
            )
        );

        // Trying to select a large amount will fail
        let amt = Amount::from_u64(1_000_000).unwrap();
        let (_orchard_notes, sapling_notes, utxos, _selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, false, false)
            .await;
        assert_eq!(sapling_notes.len(), 0);
        assert_eq!(utxos.len(), 0);

        // 4. Get an incoming transaction to a t address
        let sk = lightclient.wallet.keys().read().await.tkeys[0].clone();
        let pk = sk.pubkey().unwrap();
        let taddr = sk.address;
        let tvalue = 100_000;

        let mut fake_transaction = FakeTransaction::new(true);
        fake_transaction.add_t_output(&pk, taddr.clone(), tvalue);
        let (_ttransaction, _) = fake_compactblock_list.add_fake_transaction(fake_transaction);
        mine_pending_blocks(&mut fake_compactblock_list, &data, &lightclient).await;

        // Trying to select a large amount will now succeed
        let amt = Amount::from_u64(value + tvalue - 10_000).unwrap();
        let (_orchard_notes, sapling_notes, utxos, selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, true, false)
            .await;
        assert_eq!(selected, Amount::from_u64(value + tvalue).unwrap());
        assert_eq!(sapling_notes.len(), 1);
        assert_eq!(utxos.len(), 1);

        // If we set transparent-only = true, only the utxo should be selected
        let amt = Amount::from_u64(tvalue - 10_000).unwrap();
        let (_orchard_notes, sapling_notes, utxos, selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, true, true, false)
            .await;
        assert_eq!(selected, Amount::from_u64(tvalue).unwrap());
        assert_eq!(sapling_notes.len(), 0);
        assert_eq!(utxos.len(), 1);

        // Set min confs to 5, so the sapling note will not be selected
        lightclient.wallet.transaction_context.config.anchor_offset = [9, 4, 4, 4, 4];
        let amt = Amount::from_u64(tvalue - 10_000).unwrap();
        let (_orchard_notes, sapling_notes, utxos, selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, true, false)
            .await;
        assert_eq!(selected, Amount::from_u64(tvalue).unwrap());
        assert_eq!(sapling_notes.len(), 0);
        assert_eq!(utxos.len(), 1);
    }

    apply_scenario! {multi_z_note_selection 10}
    async fn multi_z_note_selection(scenario: NBlockFCBLScenario) {
        let NBlockFCBLScenario {
            data,
            mut lightclient,
            mut fake_compactblock_list,
            ..
        } = scenario;
        // 2. Send an incoming transaction to fill the wallet
        let extfvk1 = lightclient
            .wallet
            .keys()
            .read()
            .await
            .get_all_sapling_extfvks()[0]
            .clone();
        let value1 = 100_000;
        let (transaction, _height, _) =
            fake_compactblock_list.create_coinbase_transaction(&extfvk1, value1);
        let txid = transaction.txid();
        mine_pending_blocks(&mut fake_compactblock_list, &data, &lightclient).await;

        assert_eq!(lightclient.wallet.last_scanned_height().await, 11);

        // 3. With one confirmation, we should be able to select the note
        let amt = Amount::from_u64(10_000).unwrap();
        // Reset the anchor offsets
        lightclient.wallet.transaction_context.config.anchor_offset = [9, 4, 2, 1, 0];
        let (_orchard_notes, sapling_notes, utxos, selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, false, false)
            .await;
        assert!(selected >= amt);
        assert_eq!(sapling_notes.len(), 1);
        assert_eq!(sapling_notes[0].note.value, value1);
        assert_eq!(utxos.len(), 0);
        assert_eq!(
            incw_to_string(&sapling_notes[0].witness),
            incw_to_string(
                lightclient
                    .wallet
                    .transaction_context
                    .transaction_metadata_set
                    .read()
                    .await
                    .current
                    .get(&txid)
                    .unwrap()
                    .sapling_notes[0]
                    .witnesses
                    .last()
                    .unwrap()
            )
        );

        // Mine 5 blocks
        mine_numblocks_each_with_two_sap_txs(&mut fake_compactblock_list, &data, &lightclient, 5)
            .await;

        // 4. Send another incoming transaction.
        let value2 = 200_000;
        let (_transaction, _height, _) =
            fake_compactblock_list.create_coinbase_transaction(&extfvk1, value2);
        mine_pending_blocks(&mut fake_compactblock_list, &data, &lightclient).await;

        // Now, try to select a small amount, it should prefer the older note
        let amt = Amount::from_u64(10_000).unwrap();
        let (_orchard_notes, sapling_notes, utxos, selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, false, false)
            .await;
        assert!(selected >= amt);
        assert_eq!(sapling_notes.len(), 1);
        assert_eq!(sapling_notes[0].note.value, value1);
        assert_eq!(utxos.len(), 0);

        // Selecting a bigger amount should select both notes
        let amt = Amount::from_u64(value1 + value2).unwrap();
        let (_orchard_notes, sapling_notes, utxos, selected) = lightclient
            .wallet
            .select_notes_and_utxos(amt, false, false, false)
            .await;
        assert!(selected == amt);
        assert_eq!(sapling_notes.len(), 2);
        assert_eq!(utxos.len(), 0);
    }
    const FINAL_ROOT: &'static str =
        "1d44048a01f1c7a8958dd2927912f1c02ad10ed916877e1fd2c0a07764850a60";
    const      TREE_STATE: &'static str = "01a682706317caa5aec999385ac580445ff4eff6347e4a3c844ac18fcb5fe9bf1c01cca6f37237f27037fa7f8fe5ec8d2cc251b791cfb9cdd08cd1215229fa9435221f0001590d3e7e3f4cd572274f79f4a95b41fa72ed9b42a7c6dbcaec9637eaf368ac0e0000018843337920418307fa7699d506bb0f47a79aea7f6fe8efc1e25b9dde8966e22f013b5a8ef020d8b30fa8beb8406dd30b2a1944755f5549713e4fe24de78ab72e12000001a46523754a6d3fbc3226d6221dafca357d930e183297a0ba1cfa2db5d0500e1f01b6fd291e9d6068bc24e99aefe49f8f29836ed1223deabc23871f1a1288f9240300016fc552915a0d5bc5c0c0cdf29453edf081d9a2de396535e6084770c38dcff838019518d88883e466a41ca67d6b986739fb2f601d77bb957398ed899de70b2a9f0801cd4871c1f545e7f5d844cc65fb00b8a162e316c3d1a435b00c435032b732c4280000000000000000000000000000000000";
}
