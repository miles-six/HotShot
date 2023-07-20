//! Vote and vote accumulator types
//!
//! This module contains types used to represent the various types of votes that `HotShot` nodes
//! can send, and vote accumulator that converts votes into certificates.

use crate::{
    certificate::{QuorumCertificate, QCAssembledSignature},
    data::LeafType,
    traits::{
        election::{VoteData, VoteToken},
        node_implementation::NodeType,
        signature_key::{EncodedPublicKey, EncodedSignature},
    },
};
use commit::{Commitment, Committable};
use either::Either;
use ethereum_types::U256;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::num::NonZeroU64;

// Sishan Note: For QC signature aggregation
use bincode::Options;
use hotshot_utils::bincode::bincode_opts;
use bitvec::prelude::*;
use std::convert::TryInto;
use hotshot_primitives::{quorum_certificate::{BitvectorQuorumCertificate, QuorumCertificateValidation, StakeTableEntry, QCParams}};
use jf_primitives::signatures::{bls_over_bn254::{BLSOverBN254CurveSignatureScheme, KeyPair as QCKeyPair, VerKey as QCVerKey}};
use jf_primitives::signatures::{AggregateableSignatureSchemes, SignatureScheme};

/// The vote sent by consensus messages.
pub trait VoteType<TYPES: NodeType>:
    Debug + Clone + 'static + Serialize + for<'a> Deserialize<'a> + Send + Sync + PartialEq
{
    /// The view this vote was cast for.
    fn current_view(&self) -> TYPES::Time;
}

/// A vote on DA proposal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub struct DAVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// TODO we should remove this
    /// this is correct, but highly inefficient
    /// we should check a cache, and if that fails request the qc
    pub justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
    /// The signature share associated with this vote
    /// TODO ct/vrf make ConsensusMessage generic over I instead of serializing to a [`Vec<u8>`]
    // Sishan NOTE: signature.2 = entry including public key for QC aggregation
    pub signature: (EncodedPublicKey, EncodedSignature, StakeTableEntry<QCVerKey>),
    /// The block commitment being voted on.
    pub block_commitment: Commitment<TYPES::BlockType>,
    /// The view this vote was cast for
    pub current_view: TYPES::Time,
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
    /// The vote data this vote is signed over
    pub vote_data: VoteData<TYPES::BlockType>,
}

/// A positive or negative vote on validating or commitment proposal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub struct YesOrNoVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// TODO we should remove this
    /// this is correct, but highly inefficient
    /// we should check a cache, and if that fails request the qc
    pub justify_qc_commitment: Commitment<QuorumCertificate<TYPES, LEAF>>,
    /// The signature share associated with this vote
    /// TODO ct/vrf make ConsensusMessage generic over I instead of serializing to a [`Vec<u8>`]
    // Sishan Note: signature.2 = entry with public key for QC aggregation
    pub signature: (EncodedPublicKey, EncodedSignature, StakeTableEntry<QCVerKey>),
    /// The leaf commitment being voted on.
    pub leaf_commitment: Commitment<LEAF>,
    /// The view this vote was cast for
    pub current_view: TYPES::Time,
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
    /// The vote data this vote is signed over
    pub vote_data: VoteData<LEAF>,
}

/// A timeout vote.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub struct TimeoutVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// The justification qc for this view
    // TODO ED This should be the high_qc instead, and the signature should be over it,
    // not just over the view number
    pub justify_qc: QuorumCertificate<TYPES, LEAF>,
    /// The signature share associated with this vote
    /// TODO ct/vrf make ConsensusMessage generic over I instead of serializing to a [`Vec<u8>`]
    // Sishan NOTE: signature.2 = entry with public key for QC aggregation
    pub signature: (EncodedPublicKey, EncodedSignature, StakeTableEntry<QCVerKey>),
    /// The view this vote was cast for
    pub current_view: TYPES::Time,
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
    /// The vote data this vote is signed over
    pub vote_data: VoteData<TYPES::Time>,
}

/// The internals of a view sync vote
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub struct ViewSyncVoteInternal<TYPES: NodeType> {
    /// The relay this vote is intended for
    pub relay: EncodedPublicKey,
    /// The view number we are trying to sync on
    pub round: TYPES::Time,
    /// This node's signature over the VoteData
    pub signature: (EncodedPublicKey, EncodedSignature, StakeTableEntry<QCVerKey>),
    /// The vote token generated by this replica
    pub vote_token: TYPES::VoteTokenType,
    /// The vote data this vote is signed over
    pub vote_data: VoteData<ViewSyncData<TYPES>>,
}

/// The data View Sync votes are signed over
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Hash)]
#[serde(bound(deserialize = ""))]
pub struct ViewSyncData<TYPES: NodeType> {
    /// The relay this vote is intended for
    pub relay: EncodedPublicKey,
    /// The view number we are trying to sync on
    pub round: TYPES::Time,
}

impl<TYPES: NodeType> Committable for ViewSyncData<TYPES> {
    fn commit(&self) -> Commitment<Self> {
        let builder = commit::RawCommitmentBuilder::new("Quorum Certificate Commitment");

        builder
            .var_size_field("Relay public key", &self.relay.0)
            .u64(*self.round)
            .finalize()
    }
}

/// Votes to synchronize the network on a single view
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub enum ViewSyncVote<TYPES: NodeType> {
    /// PreCommit vote
    PreCommit(ViewSyncVoteInternal<TYPES>),
    /// Commit vote
    Commit(ViewSyncVoteInternal<TYPES>),
    /// Finalize vote
    Finalize(ViewSyncVoteInternal<TYPES>),
}

/// Votes on validating or commitment proposal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(bound(deserialize = ""))]
pub enum QuorumVote<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// Posivite vote.
    Yes(YesOrNoVote<TYPES, LEAF>),
    /// Negative vote.
    No(YesOrNoVote<TYPES, LEAF>),
    /// Timeout vote.
    Timeout(TimeoutVote<TYPES, LEAF>),
}

impl<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> VoteType<TYPES> for DAVote<TYPES, LEAF> {
    fn current_view(&self) -> TYPES::Time {
        self.current_view
    }
}

impl<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> VoteType<TYPES>
    for QuorumVote<TYPES, LEAF>
{
    fn current_view(&self) -> TYPES::Time {
        match self {
            QuorumVote::Yes(v) | QuorumVote::No(v) => v.current_view,
            QuorumVote::Timeout(v) => v.current_view,
        }
    }
}

impl<TYPES: NodeType> VoteType<TYPES> for ViewSyncVote<TYPES> {
    fn current_view(&self) -> TYPES::Time {
        match self {
            ViewSyncVote::PreCommit(v) | ViewSyncVote::Commit(v) | ViewSyncVote::Finalize(v) => {
                v.round
            }
        }
    }
}

/// The aggreation of votes, implemented by `VoteAccumulator`.
pub trait Accumulator<T, U>: Sized {
    /// Accumate the `val` to the current state.
    ///
    /// If a threshold is reached, returns `U` (e.g., a certificate). Else, returns `Self` and
    /// continues accumulating items.
    fn append(self, val: T) -> Either<Self, U>;
}

/// Mapping of commitments to vote tokens by key.
type VoteMap<C, TOKEN> = HashMap<
    Commitment<C>,
    (
        u64,
        BTreeMap<EncodedPublicKey, (EncodedSignature, VoteData<C>, TOKEN)>,
    ),
>;

/// Describe the process of collecting signatures on block or leaf commitment, to form a DAC or QC,
/// respectively.
// TODO ED Change LEAF to COMMITTABLE
pub struct VoteAccumulator<TOKEN, LEAF: Committable + Serialize + Clone> {
    /// Map of all signatures accumlated so far
    pub total_vote_outcomes: VoteMap<LEAF, TOKEN>,
    /// Map of all da signatures accumlated so far
    pub da_vote_outcomes: VoteMap<LEAF, TOKEN>,
    /// Map of all yes signatures accumlated so far
    pub yes_vote_outcomes: VoteMap<LEAF, TOKEN>,
    /// Map of all no signatures accumlated so far
    pub no_vote_outcomes: VoteMap<LEAF, TOKEN>,
    /// A quorum's worth of stake, generall 2f + 1
    pub success_threshold: NonZeroU64,
    /// Enough stake to know that we cannot possibly get a quorum, generally f + 1
    pub failure_threshold: NonZeroU64,
    // Sishan NOTE: For QC aggregation
    // a list of signatures
    pub sig_lists: Vec<<BLSOverBN254CurveSignatureScheme as SignatureScheme>::Signature>,
    // bitvec to indicate which node is active
    pub signers: BitVec,
}

impl<TOKEN, LEAF: Committable + Serialize + Clone>
    Accumulator<
        (
            Commitment<LEAF>,
            (EncodedPublicKey, (EncodedSignature, StakeTableEntry<QCVerKey>, Vec<StakeTableEntry<QCVerKey>>,  usize, VoteData<LEAF>, TOKEN)),
        ),
        QCAssembledSignature,
    > for VoteAccumulator<TOKEN, LEAF>
where
    TOKEN: Clone + VoteToken,
{
    fn append(
        mut self,
        val: (
            Commitment<LEAF>,
            (EncodedPublicKey, (EncodedSignature, StakeTableEntry<QCVerKey>, Vec<StakeTableEntry<QCVerKey>>, usize, VoteData<LEAF>, TOKEN)),
        ),
    ) -> Either<Self, QCAssembledSignature> {
        let (commitment, (key, (sig, entry, entries, node_id, vote_data, token))) = val;

        // Sishan NOTE: Desereialize the sig so that it can be assembeld into a QC
        let origianl_sig: <BLSOverBN254CurveSignatureScheme as SignatureScheme>::Signature 
        = bincode_opts().deserialize(&sig.clone().0).unwrap();

        // update the active_keys and sig_lists
        self.signers.set(node_id, true);
        self.sig_lists.push(origianl_sig.clone());

        let (total_stake_casted, total_vote_map) = self
            .total_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (da_stake_casted, da_vote_map) = self
            .da_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (yes_stake_casted, yes_vote_map) = self
            .yes_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        let (no_stake_casted, no_vote_map) = self
            .no_vote_outcomes
            .entry(commitment)
            .or_insert_with(|| (0, BTreeMap::new()));

        // Accumulate the stake for each leaf commitment rather than the total
        // stake of all votes, in case they correspond to inconsistent
        // commitments.
        *total_stake_casted += u64::from(token.vote_count());
        total_vote_map.insert(key.clone(), (sig.clone(), vote_data.clone(), token.clone()));

        match vote_data {
            VoteData::DA(_) => {
                *da_stake_casted += u64::from(token.vote_count());
                da_vote_map.insert(key, (sig.clone(), vote_data, token));
            }
            VoteData::Yes(_) => {
                *yes_stake_casted += u64::from(token.vote_count());
                yes_vote_map.insert(key, (sig.clone(), vote_data, token));
            }
            VoteData::No(_) => {
                *no_stake_casted += u64::from(token.vote_count());
                no_vote_map.insert(key, (sig.clone(), vote_data, token));
            }
            VoteData::Timeout(_) => {
                unimplemented!()
            }
            VoteData::ViewSync(_) => todo!(),
        }

        if *total_stake_casted >= u64::from(self.success_threshold) {
            
            // Sishan NOTE: Do assemble for QC here

            let real_qc_pp = QCParams {
                stake_entries: entries.clone(),
                threshold: U256::from(self.success_threshold.get()),
                agg_sig_pp: (),
            };

            let real_qc_sig = BitvectorQuorumCertificate::<BLSOverBN254CurveSignatureScheme>::assemble(
                &real_qc_pp,
                self.signers.as_bitslice(),
                &self.sig_lists[..],
            ).unwrap();

            


            if *da_stake_casted >= u64::from(self.success_threshold) {
                self.da_vote_outcomes.remove(&commitment).unwrap().1;
                return Either::Right(QCAssembledSignature::DA(real_qc_sig));
            } else if *yes_stake_casted >= u64::from(self.success_threshold) {
                self.yes_vote_outcomes.remove(&commitment).unwrap().1;
                return Either::Right(QCAssembledSignature::Yes(real_qc_sig));
            } else if *no_stake_casted >= u64::from(self.failure_threshold) {
                self.total_vote_outcomes.remove(&commitment).unwrap().1;
                return Either::Right(QCAssembledSignature::No(real_qc_sig));
            }
        }
        Either::Left(self)
    }
}
