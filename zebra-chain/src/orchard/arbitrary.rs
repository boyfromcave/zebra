//! Randomised data generation for Orchard types.

use group::{ff::PrimeField, prime::PrimeCurveAffine};
use halo2::{arithmetic::FieldExt, pasta::pallas};
use proptest::{arbitrary::any, array, collection::vec, prelude::*};

use reddsa::{orchard::SpendAuth, Signature, SigningKey, VerificationKey, VerificationKeyBytes};

use super::{
    keys::*, note, tree, Action, AuthorizedAction, Flags, NoteCommitment, ValueCommitment,
};

impl Arbitrary for Action {
    type Parameters = ();

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (
            any::<note::Nullifier>(),
            any::<SpendAuthVerificationKeyBytes>(),
            any::<note::EncryptedNote>(),
            any::<note::WrappedNoteKey>(),
        )
            .prop_map(|(nullifier, rk, enc_ciphertext, out_ciphertext)| Self {
                cv: ValueCommitment(pallas::Affine::identity()),
                nullifier,
                rk: rk.0,
                cm_x: NoteCommitment(pallas::Affine::identity()).extract_x(),
                ephemeral_key: EphemeralPublicKey(pallas::Affine::generator()),
                enc_ciphertext,
                out_ciphertext,
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for note::Nullifier {
    type Parameters = ();

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (vec(any::<u8>(), 64))
            .prop_map(|bytes| {
                let bytes = bytes.try_into().expect("vec is the correct length");
                Self::try_from(pallas::Scalar::from_bytes_wide(&bytes).to_repr())
                    .expect("a valid generated nullifier")
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for AuthorizedAction {
    type Parameters = ();

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (any::<Action>(), any::<SpendAuthSignature>())
            .prop_map(|(action, spend_auth_sig)| Self {
                action,
                spend_auth_sig: spend_auth_sig.0,
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct SpendAuthSignature(pub(crate) Signature<SpendAuth>);

impl Arbitrary for SpendAuthSignature {
    type Parameters = ();

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (array::uniform32(any::<u8>()), array::uniform32(any::<u8>()))
            .prop_map(|(r_bytes, s_bytes)| {
                let mut bytes = [0; 64];
                bytes[0..32].copy_from_slice(&r_bytes[..]);
                bytes[32..64].copy_from_slice(&s_bytes[..]);
                SpendAuthSignature(Signature::<SpendAuth>::from(bytes))
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct SpendAuthVerificationKeyBytes(pub(crate) VerificationKeyBytes<SpendAuth>);

impl Arbitrary for SpendAuthVerificationKeyBytes {
    type Parameters = ();

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        // Generate a random signing key from a "seed".
        (vec(any::<u8>(), 64))
            .prop_map(|bytes| {
                let bytes = bytes.try_into().expect("vec is the correct length");
                // Convert to a scalar
                let sk_scalar = pallas::Scalar::from_bytes_wide(&bytes);
                // Convert that back to a (canonical) encoding
                let sk_bytes = sk_scalar.to_repr();
                // Decode it into a signing key
                let sk = SigningKey::try_from(sk_bytes).unwrap();
                let pk = VerificationKey::<SpendAuth>::from(&sk);
                SpendAuthVerificationKeyBytes(pk.into())
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for Flags {
    type Parameters = ();

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (any::<u8>()).prop_map(Self::from_bits_truncate).boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}

impl Arbitrary for tree::Root {
    type Parameters = ();

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        (vec(any::<u8>(), 64))
            .prop_map(|bytes| {
                let bytes = bytes.try_into().expect("vec is the correct length");
                Self::try_from(pallas::Base::from_bytes_wide(&bytes).to_repr())
                    .expect("a valid generated Orchard note commitment tree root")
            })
            .boxed()
    }

    type Strategy = BoxedStrategy<Self>;
}
