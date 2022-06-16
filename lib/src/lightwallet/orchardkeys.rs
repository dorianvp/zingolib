use orchard::keys::{Diversifier, FullViewingKey, IncomingViewingKey, OutgoingViewingKey, Scope, SpendingKey};
use zcash_address::unified::{Address as UnifiedAddress, Encoding, Receiver, Typecode};
// A struct that holds orchard private keys or view keys
#[derive(Clone, Debug, PartialEq)]
pub struct WalletOKey {
    pub(super) key: WalletOKeyInner,
    locked: bool,
    pub(super) unified_address: UnifiedAddress,

    // If this is a HD key, what is the key number
    pub(super) hdkey_num: Option<u32>,

    // If locked, the encrypted private key is stored here
    enc_key: Option<Vec<u8>>,
    nonce: Option<Vec<u8>>,
}

impl WalletOKey {
    pub fn new_imported_osk(key: SpendingKey) -> Self {
        Self {
            key: WalletOKeyInner::ImportedSpendingKey(key),
            locked: false,
            unified_address: UnifiedAddress::try_from_items(vec![Receiver::Orchard(
                FullViewingKey::from(&key)
                    .address_at(0u32, Scope::Internal)
                    .to_raw_address_bytes(),
            )])
            .unwrap(),
            hdkey_num: None,
            enc_key: None,
            nonce: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum WalletOKeyInner {
    HdKey(SpendingKey),
    ImportedSpendingKey(SpendingKey),
    ImportedFullViewKey(FullViewingKey),
    ImportedInViewKey(IncomingViewingKey),
    ImportedOutViewKey(OutgoingViewingKey),
}

impl WalletOKeyInner {
    pub(crate) fn spending_key(&self) -> Option<SpendingKey> {
        match self {
            Self::HdKey(k) => Some(*k),
            Self::ImportedSpendingKey(k) => Some(*k),
            _ => None,
        }
    }
    pub(crate) fn full_viewing_key(&self) -> Option<FullViewingKey> {
        match self {
            Self::HdKey(k) => Some(FullViewingKey::from(k)),
            Self::ImportedSpendingKey(k) => Some(FullViewingKey::from(k)),
            Self::ImportedFullViewKey(k) => Some(k.clone()),
            _ => None,
        }
    }
}

impl PartialEq for WalletOKeyInner {
    fn eq(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq as _;
        use WalletOKeyInner::*;
        match (self, other) {
            (HdKey(a), HdKey(b)) => bool::from(a.ct_eq(b)),
            (ImportedSpendingKey(a), ImportedSpendingKey(b)) => bool::from(a.ct_eq(b)),
            (ImportedFullViewKey(a), ImportedFullViewKey(b)) => a == b,
            (ImportedInViewKey(a), ImportedInViewKey(b)) => a == b,
            (ImportedOutViewKey(a), ImportedOutViewKey(b)) => a.as_ref() == b.as_ref(),
            _ => false,
        }
    }
}
impl WalletOKey {
    pub fn new_hdkey(hdkey_num: u32, spending_key: SpendingKey) -> Self {
        let key = WalletOKeyInner::HdKey(spending_key);
        let address = FullViewingKey::from(&spending_key).address_at(0u64, Scope::Internal);
        let orchard_container = Receiver::Orchard(address.to_raw_address_bytes());
        let unified_address = UnifiedAddress::try_from_items(vec![orchard_container]).unwrap();

        WalletOKey {
            key,
            locked: false,
            unified_address,
            hdkey_num: Some(hdkey_num),
            enc_key: None,
            nonce: None,
        }
    }
}