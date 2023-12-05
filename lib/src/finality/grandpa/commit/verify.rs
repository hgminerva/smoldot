// Smoldot
// Copyright (C) 2019-2022  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use crate::finality::grandpa::commit::decode;

use alloc::vec::Vec;
use core::{cmp, iter, mem};
use rand_chacha::{
    rand_core::{RngCore as _, SeedableRng as _},
    ChaCha20Rng,
};

/// Configuration for a commit verification process.
#[derive(Debug)]
pub struct Config<C> {
    /// SCALE-encoded commit to verify.
    pub commit: C,

    /// Number of bytes used for encoding the block number in the SCALE-encoded commit.
    pub block_number_bytes: usize,

    // TODO: document
    pub expected_authorities_set_id: u64,

    /// Number of authorities that are allowed to emit pre-commits. Used to calculate the
    /// threshold of the number of required signatures.
    pub num_authorities: u32,

    /// Seed for a PRNG used for various purposes during the verification.
    ///
    /// > **Note**: The verification is nonetheless deterministic.
    pub randomness_seed: [u8; 32],
}

/// Commit verification in progress.
#[must_use]
pub enum InProgress<C> {
    /// See [`IsAuthority`].
    IsAuthority(IsAuthority<C>),
    /// See [`IsParent`].
    IsParent(IsParent<C>),
    /// Verification is finished. Contains an error if the commit message is invalid.
    Finished(Result<(), Error>),
    /// Verification is finished, but [`IsParent::resume`] has been called with `None`, meaning
    /// that some signatures couldn't be verified, and the commit message doesn't contain enough
    /// signatures that are known to be valid.
    ///
    /// The commit must be verified again after more blocks are available.
    FinishedUnknown,
}

/// Verifies that a commit is valid.
pub fn verify<C: AsRef<[u8]>>(config: Config<C>) -> InProgress<C> {
    let decoded_commit =
        match decode::decode_grandpa_commit(config.commit.as_ref(), config.block_number_bytes) {
            Ok(c) => c,
            Err(_) => return InProgress::Finished(Err(Error::InvalidFormat)),
        };

    if decoded_commit.set_id != config.expected_authorities_set_id {
        return InProgress::Finished(Err(Error::BadSetId));
    }

    if decoded_commit.auth_data.len() != decoded_commit.precommits.len() {
        return InProgress::Finished(Err(Error::InvalidFormat));
    }

    let mut randomness = ChaCha20Rng::from_seed(config.randomness_seed);

    // Make sure that there is no duplicate authority public key.
    {
        let mut unique = hashbrown::HashSet::with_capacity_and_hasher(
            decoded_commit.auth_data.len(),
            crate::util::SipHasherBuild::new({
                let mut seed = [0; 16];
                randomness.fill_bytes(&mut seed);
                seed
            }),
        );
        if let Some((_, faulty_pub_key)) = decoded_commit
            .auth_data
            .iter()
            .find(|(_, pubkey)| !unique.insert(pubkey))
        {
            return InProgress::Finished(Err(Error::DuplicateSignature(**faulty_pub_key)));
        }
    }

    Verification {
        commit: config.commit,
        block_number_bytes: config.block_number_bytes,
        next_precommit_index: 0,
        next_precommit_author_verified: false,
        next_precommit_block_verified: false,
        num_verified_signatures: 0,
        num_authorities: config.num_authorities,
        signatures_batch: ed25519_zebra::batch::Verifier::new(),
        randomness,
    }
    .resume()
}

/// Must return whether a certain public key is in the list of authorities that are allowed to
/// generate pre-commits.
#[must_use]
pub struct IsAuthority<C> {
    inner: Verification<C>,
}

impl<C: AsRef<[u8]>> IsAuthority<C> {
    /// Public key to verify.
    pub fn authority_public_key(&self) -> &[u8; 32] {
        debug_assert!(!self.inner.next_precommit_author_verified);
        let decoded_commit = decode::decode_grandpa_commit(
            self.inner.commit.as_ref(),
            self.inner.block_number_bytes,
        )
        .unwrap();
        decoded_commit.auth_data[self.inner.next_precommit_index].1
    }

    /// Resumes the verification process.
    ///
    /// Must be passed `true` if the public key is indeed in the list of authorities.
    /// Passing `false` always returns [`InProgress::Finished`] containing an error.
    pub fn resume(mut self, is_authority: bool) -> InProgress<C> {
        if !is_authority {
            let key = *self.authority_public_key();
            return InProgress::Finished(Err(Error::NotAuthority(key)));
        }

        self.inner.next_precommit_author_verified = true;
        self.inner.resume()
    }
}

/// Must return whether a certain block is a descendant of the target block.
#[must_use]
pub struct IsParent<C> {
    inner: Verification<C>,
    /// For performance reasons, the block number is copied here, but not the block hash. This
    /// hasn't actually been benchmarked, so feel free to do so.
    block_number: u64,
}

impl<C: AsRef<[u8]>> IsParent<C> {
    /// Height of the block to check.
    pub fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Hash of the block to check.
    pub fn block_hash(&self) -> &[u8; 32] {
        debug_assert!(!self.inner.next_precommit_block_verified);
        let decoded_commit = decode::decode_grandpa_commit(
            self.inner.commit.as_ref(),
            self.inner.block_number_bytes,
        )
        .unwrap();
        decoded_commit.precommits[self.inner.next_precommit_index].target_hash
    }

    /// Height of the block that must be the ancestor of the block to check.
    pub fn target_block_number(&self) -> u64 {
        let decoded_commit = decode::decode_grandpa_commit(
            self.inner.commit.as_ref(),
            self.inner.block_number_bytes,
        )
        .unwrap();
        decoded_commit.target_number
    }

    /// Hash of the block that must be the ancestor of the block to check.
    pub fn target_block_hash(&self) -> &[u8; 32] {
        let decoded_commit = decode::decode_grandpa_commit(
            self.inner.commit.as_ref(),
            self.inner.block_number_bytes,
        )
        .unwrap();
        decoded_commit.target_hash
    }

    /// Resumes the verification process.
    ///
    /// Must be passed `Some(true)` if the block is known to be a descendant of the target block,
    /// or `None` if it is unknown.
    /// Passing `Some(false)` always returns [`InProgress::Finished`] containing an error.
    pub fn resume(mut self, is_parent: Option<bool>) -> InProgress<C> {
        match is_parent {
            None => {}
            Some(true) => self.inner.num_verified_signatures += 1,
            Some(false) => {
                return InProgress::Finished(Err(Error::BadAncestry));
            }
        }

        self.inner.next_precommit_block_verified = true;
        self.inner.resume()
    }
}

struct Verification<C> {
    /// Encoded commit message. Guaranteed to decode successfully.
    commit: C,

    /// See [`Config::block_number_bytes`].
    block_number_bytes: usize,

    /// Index of the next pre-commit to process within the commit.
    next_precommit_index: usize,

    /// Whether the precommit whose index is [`Verification::next_precommit_index`] has been
    /// verified as coming from the list of authorities.
    next_precommit_author_verified: bool,

    /// Whether the precommit whose index is [`Verification::next_precommit_index`] has been
    /// verified to be about a block that is a descendant of the target block.
    next_precommit_block_verified: bool,

    /// Number of signatures that have been pushed for verification. Needs to be above a certain
    /// threshold for the commit to be valid.
    num_verified_signatures: usize,

    /// Number of authorities in the list. Used to calculate the threshold of the number of
    /// required signatures.
    num_authorities: u32,

    /// Verifying all the signatures together brings better performances than verifying them one
    /// by one.
    /// Note that batched Ed25519 verification has some issues. The code below uses a special
    /// flavor of Ed25519 where ambiguities are removed.
    /// See <https://docs.rs/ed25519-zebra/2.2.0/ed25519_zebra/batch/index.html> and
    /// <https://github.com/zcash/zips/blob/master/zip-0215.rst>
    signatures_batch: ed25519_zebra::batch::Verifier,

    /// Randomness generator used during the batch verification.
    randomness: ChaCha20Rng,
}

impl<C: AsRef<[u8]>> Verification<C> {
    fn resume(mut self) -> InProgress<C> {
        // The `verify` function that starts the verification performs the preliminary check that
        // the commit has the correct format.
        let decoded_commit =
            decode::decode_grandpa_commit(self.commit.as_ref(), self.block_number_bytes).unwrap();

        loop {
            if let Some(precommit) = decoded_commit.precommits.get(self.next_precommit_index) {
                if !self.next_precommit_author_verified {
                    return InProgress::IsAuthority(IsAuthority { inner: self });
                }

                if !self.next_precommit_block_verified {
                    if precommit.target_hash == decoded_commit.target_hash
                        && precommit.target_number == decoded_commit.target_number
                    {
                        self.next_precommit_block_verified = true;
                    } else {
                        return InProgress::IsParent(IsParent {
                            block_number: precommit.target_number,
                            inner: self,
                        });
                    }
                }

                let authority_public_key = decoded_commit.auth_data[self.next_precommit_index].1;
                let signature = decoded_commit.auth_data[self.next_precommit_index].0;

                let mut msg = Vec::with_capacity(1 + 32 + self.block_number_bytes + 8 + 8);
                msg.push(1u8); // This `1` indicates which kind of message is being signed.
                msg.extend_from_slice(&precommit.target_hash[..]);
                // The message contains the little endian block number. While simple in concept,
                // in reality it is more complicated because we don't know the number of bytes of
                // this block number at compile time. We thus copy as many bytes as appropriate and
                // pad with 0s if necessary.
                msg.extend_from_slice(
                    &precommit.target_number.to_le_bytes()[..cmp::min(
                        mem::size_of_val(&precommit.target_number),
                        self.block_number_bytes,
                    )],
                );
                msg.extend(
                    iter::repeat(0).take(
                        self.block_number_bytes
                            .saturating_sub(mem::size_of_val(&precommit.target_number)),
                    ),
                );
                msg.extend_from_slice(&u64::to_le_bytes(decoded_commit.round_number)[..]);
                msg.extend_from_slice(&u64::to_le_bytes(decoded_commit.set_id)[..]);
                debug_assert_eq!(msg.len(), msg.capacity());

                self.signatures_batch
                    .queue(ed25519_zebra::batch::Item::from((
                        ed25519_zebra::VerificationKeyBytes::from(*authority_public_key),
                        ed25519_zebra::Signature::from(*signature),
                        &msg,
                    )));

                self.next_precommit_index += 1;
                self.next_precommit_author_verified = false;
                self.next_precommit_block_verified = false;
            } else {
                debug_assert!(!self.next_precommit_author_verified);
                debug_assert!(!self.next_precommit_block_verified);

                // Check that commit contains a number of signatures equal to at least 2/3rd of the
                // number of authorities.
                // Duplicate signatures are checked below.
                // The logic of the check is `actual >= (expected * 2 / 3) + 1`.
                if decoded_commit.precommits.len()
                    < (usize::try_from(self.num_authorities).unwrap() * 2 / 3) + 1
                {
                    return InProgress::FinishedUnknown;
                }

                // Actual signatures verification performed here.
                match self.signatures_batch.verify(&mut self.randomness) {
                    Ok(()) => {}
                    Err(_) => return InProgress::Finished(Err(Error::BadSignature)),
                }

                return InProgress::Finished(Ok(()));
            }
        }
    }
}

/// Error that can happen while verifying a commit.
#[derive(Debug, derive_more::Display)]
pub enum Error {
    /// Failed to decode the commit message.
    InvalidFormat,
    /// The authorities set id of the commit doesn't match the one that is expected.
    BadSetId,
    /// One of the public keys is invalid.
    BadPublicKey,
    /// One of the signatures can't be verified.
    BadSignature,
    /// One authority has produced two signatures.
    #[display(fmt = "One authority has produced two signatures")]
    DuplicateSignature([u8; 32]),
    /// One of the public keys isn't in the list of authorities.
    #[display(fmt = "One of the public keys isn't in the list of authorities")]
    NotAuthority([u8; 32]),
    /// Commit contains a vote for a block that isn't a descendant of the target block.
    BadAncestry,
}

// TODO: tests
