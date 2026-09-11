//! The types of transaction Proposal that Zingo! uses.

use std::convert::Infallible;

use zcash_client_backend::proposal::Proposal;
use zcash_primitives::transaction::fees::zip317;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::value::{BalanceError, Zatoshis};
use zcash_transparent::address::TransparentAddress;

use pepper_sync::keys::transparent::TransparentAddressId;

use crate::wallet::output::OutputRef;
use crate::wallet::transparent::OpReturnData;

/// A proposed send to addresses.
/// Identifies the notes to spend by txid, pool, and `output_index`.
/// This type alias, specifies the ZIP317 "Proportional Transfer Fee Mechanism" structure
/// <https://zips.z.cash/zip-0317>
/// as the fee structure for a transaction series. This innovation was created in response
/// "Binance Constraint" that t-addresses that only receive from t-addresses be supported.
/// <https://zips.z.cash/zip-0320>
pub(crate) type ProportionalFeeProposal = Proposal<zip317::FeeRule, OutputRef>;

/// A proposed shielding.
/// The `zcash_client_backend` Proposal type exposes a "`NoteRef`" generic
/// parameter to track Shielded inputs to the proposal these are
/// disallowed in Zingo `ShieldedProposals`
pub(crate) type ProportionalFeeShieldProposal = Proposal<zip317::FeeRule, Infallible>;

/// The `LightClient` holds one proposal at a time while the user decides whether to accept the fee.
#[derive(Debug, Clone)]
pub(crate) enum ZingoProposal {
    /// Send proposal.
    Send {
        proposal: ProportionalFeeProposal,
        sending_account: zip32::AccountId,
    },
    /// Shield proposal.
    Shield {
        proposal: ProportionalFeeShieldProposal,
        shielding_account: zip32::AccountId,
    },
    /// OP_RETURN send proposal.
    OpReturn(OpReturnProposal),
}

/// A proposed OP_RETURN send. It is split into two transactions.
///
/// The first is the deshield. It moves `amount` plus `op_return_fee` from
/// shielded funds to `source_address`, an ephemeral transparent address
/// reserved for this proposal. The second is the OP_RETURN send. It spends
/// that output to `recipient` and carries `data` in a null-data output.
/// The second transaction has no change output.
///
/// The second transaction cannot be built before the first exists. Its
/// fee is fixed at proposal time and is reported by
/// [`OpReturnProposal::op_return_fee`].
#[derive(Debug, Clone)]
pub struct OpReturnProposal {
    deshield: ProportionalFeeProposal,
    sending_account: zip32::AccountId,
    source_address_id: TransparentAddressId,
    source_address: TransparentAddress,
    recipient: TransparentAddress,
    amount: Zatoshis,
    data: OpReturnData,
    op_return_fee: Zatoshis,
    target_height: BlockHeight,
}

impl OpReturnProposal {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        deshield: ProportionalFeeProposal,
        sending_account: zip32::AccountId,
        source_address_id: TransparentAddressId,
        source_address: TransparentAddress,
        recipient: TransparentAddress,
        amount: Zatoshis,
        data: OpReturnData,
        op_return_fee: Zatoshis,
        target_height: BlockHeight,
    ) -> Self {
        Self {
            deshield,
            sending_account,
            source_address_id,
            source_address,
            recipient,
            amount,
            data,
            op_return_fee,
            target_height,
        }
    }

    /// The deshield proposal. Its single payment is `amount` plus
    /// [`Self::op_return_fee`] to the ephemeral source address.
    pub fn deshield(&self) -> &ProportionalFeeProposal {
        &self.deshield
    }

    /// The account the deshield spends from.
    pub fn sending_account(&self) -> zip32::AccountId {
        self.sending_account
    }

    pub(crate) fn source_address_id(&self) -> TransparentAddressId {
        self.source_address_id
    }

    /// The ephemeral transparent address the deshield pays and the
    /// OP_RETURN send spends.
    pub fn source_address(&self) -> &TransparentAddress {
        &self.source_address
    }

    /// The transparent recipient of the OP_RETURN send.
    pub fn recipient(&self) -> &TransparentAddress {
        &self.recipient
    }

    /// The amount the recipient receives.
    pub fn amount(&self) -> Zatoshis {
        self.amount
    }

    /// The OP_RETURN payload.
    pub fn data(&self) -> &OpReturnData {
        &self.data
    }

    /// The ZIP-317 fee of the deshield.
    pub fn deshield_fee(&self) -> Result<Zatoshis, BalanceError> {
        total_fee(&self.deshield)
    }

    /// The ZIP-317 fee of the OP_RETURN send.
    pub fn op_return_fee(&self) -> Zatoshis {
        self.op_return_fee
    }

    /// The fee of both transactions.
    pub fn total_fee(&self) -> Result<Zatoshis, BalanceError> {
        (self.deshield_fee()? + self.op_return_fee).ok_or(BalanceError::Overflow)
    }

    pub(crate) fn target_height(&self) -> BlockHeight {
        self.target_height
    }
}

/// total sum of all transaction request payment amounts in a proposal
pub fn total_payment_amount(proposal: &ProportionalFeeProposal) -> Result<Zatoshis, BalanceError> {
    proposal
        .steps()
        .iter()
        .map(zcash_client_backend::proposal::Step::transaction_request)
        .try_fold(Zatoshis::ZERO, |acc, request| {
            // zip321 payment amounts are now optional; unspecified amounts contribute zero.
            let request_total = request.total()?.unwrap_or(Zatoshis::ZERO);
            (acc + request_total).ok_or(BalanceError::Overflow)
        })
}

/// total sum of all fees in a proposal
pub fn total_fee(proposal: &ProportionalFeeProposal) -> Result<Zatoshis, BalanceError> {
    proposal
        .steps()
        .iter()
        .map(|step| step.balance().fee_required())
        .try_fold(Zatoshis::ZERO, |acc, fee| {
            (acc + fee).ok_or(BalanceError::Overflow)
        })
}

#[cfg(test)]
mod tests {
    use zcash_protocol::value::Zatoshis;

    use crate::mocks;

    #[test]
    fn total_payment_amount() {
        let proposal = mocks::proposal::ProposalBuilder::default().build();
        assert_eq!(
            super::total_payment_amount(&proposal).unwrap(),
            Zatoshis::from_u64(100_000).unwrap()
        );
    }
    #[test]
    fn total_fee() {
        let proposal = mocks::proposal::ProposalBuilder::default().build();
        assert_eq!(
            super::total_fee(&proposal).unwrap(),
            Zatoshis::from_u64(20_000).unwrap()
        );
    }
}
