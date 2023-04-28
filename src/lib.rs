#![cfg_attr(not(feature = "std"), no_std)]
#![warn(future_incompatible, nonstandard_style)]
#![cfg_attr(not(feature = "blst"), deny(unsafe_code))]
use ark_ec::{pairing::Pairing, scalar_mul::fixed_base::FixedBase, CurveGroup, ScalarMul};
use ark_ff::{Field, PrimeField};
use ark_poly::{
    univariate::{DenseOrSparsePolynomial, DensePolynomial},
    DenseUVPolynomial, EvaluationDomain, GeneralEvaluationDomain,
};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize, Compress, SerializationError};
use ark_std::{vec, vec::Vec};
use merlin::Transcript;
#[cfg(test)]
use rand::thread_rng as test_rng;

// Public uses
pub use ark_ff;
pub use ark_poly;
pub use ark_serialize;
pub use merlin;

pub mod method1;
pub mod method2;

pub mod lagrange;
#[cfg(feature = "blst")]
pub mod m1_blst;

pub mod traits;

#[cfg(test)]
pub mod testing;

#[derive(Debug, Eq, PartialEq)]
#[cfg_attr(feature = "std", derive(thiserror::Error))]
pub enum Error {
    #[cfg_attr(feature = "std", error("Polynomial given is too large"))]
    TooManyScalars {
        n_coeffs: usize,
        expected_max: usize,
    },
    #[cfg_attr(feature = "std", error("A divisor was zero"))]
    DivisorIsZero,
    #[cfg_attr(feature = "std", error("Expected polynomials, none were given"))]
    NoPolynomialsGiven,
    #[cfg_attr(feature = "std", error("Given evaluations were the incorrect size"))]
    EvalsIncorrectSize {
        poly: usize,
        n: usize,
        expected: usize,
    },
    #[cfg_attr(feature = "std", error("Serialization error"))]
    SerializationError,
    #[cfg_attr(feature = "std", error("Not given any points"))]
    NoPointsGiven,
    #[cfg_attr(
        feature = "std",
        error("Given {n_eval_rows} evaluations, but {n_polys} polynomials")
    )]
    EvalsAndPolysDifferentSizes { n_eval_rows: usize, n_polys: usize },
    #[cfg_attr(feature = "std", error("Given {n_points} points, but {n_evals} evals"))]
    EvalsAndPointsDifferentSizes { n_points: usize, n_evals: usize },
    #[cfg_attr(
        feature = "std",
        error("Given {n_commits} commits, but {n_evals} evals")
    )]
    EvalsAndCommitsDifferentSizes { n_evals: usize, n_commits: usize },
    #[cfg_attr(feature = "std", error("Unable to construct a domain of size {0}"))]
    DomainConstructionFailed(usize),
}

impl From<SerializationError> for Error {
    fn from(_: SerializationError) -> Self {
        Self::SerializationError
    }
}

#[derive(Debug, Clone, CanonicalSerialize, CanonicalDeserialize)]
pub struct Commitment<E: Pairing>(pub E::G1Affine);

impl<E: Pairing> Commitment<E> {
    pub fn extend_commitments(
        commits: impl AsRef<[Commitment<E>]>,
        output_size: usize,
    ) -> Result<Vec<Self>, Error> {
        let mut vals: Vec<E::G1> = commits
            .as_ref()
            .iter()
            .map(|x| x.0.into())
            .collect::<Vec<_>>();
        let domain = GeneralEvaluationDomain::<E::ScalarField>::new(vals.len())
            .ok_or(Error::DomainConstructionFailed(vals.len()))?;
        let domain_ext = GeneralEvaluationDomain::<E::ScalarField>::new(output_size)
            .ok_or(Error::DomainConstructionFailed(output_size))?;
        domain.ifft_in_place(&mut vals);
        domain_ext.fft_in_place(&mut vals);
        Ok(vals
            .into_iter()
            .map(|x| Commitment(x.into()))
            .collect::<Vec<_>>())
    }
}

pub(crate) fn gen_powers<F: Field>(element: F, len: usize) -> Vec<F> {
    let mut powers = vec![F::one(); len];
    for i in 1..len {
        powers[i] = element * powers[i - 1];
    }
    powers
}

#[inline]
pub(crate) fn curve_msm<G: ScalarMul + CurveGroup>(
    bases: &[G::Affine],
    scalars: &[G::ScalarField],
) -> Result<G, Error> {
    if scalars.len() > bases.len() {
        return Err(Error::TooManyScalars {
            n_coeffs: scalars.len(),
            expected_max: bases.len(),
        });
    }
    let scalars = scalars.iter().map(|x| x.into_bigint()).collect::<Vec<_>>();
    let sp = G::msm_bigint(&bases[..scalars.len()], &scalars);
    Ok(sp)
}

pub(crate) fn vanishing_polynomial<F: Field>(points: impl AsRef<[F]>) -> DensePolynomial<F> {
    let one = DensePolynomial::from_coefficients_vec(vec![F::one()]);
    points
        .as_ref()
        .iter()
        .map(|&point| DensePolynomial::from_coefficients_vec(vec![-point, F::one()]))
        .fold(one, |x, y| x.naive_mul(&y))
}

/// Does polynomial division, returning q, r
pub(crate) fn poly_div_q_r<F: Field>(
    num: DenseOrSparsePolynomial<F>,
    denom: DenseOrSparsePolynomial<F>,
) -> Result<(Vec<F>, Vec<F>), Error> {
    if denom.is_zero() {
        return Err(Error::DivisorIsZero);
    }
    let (q, r) = num.divide_with_q_and_r(&denom).expect("Cannot return none");
    Ok((q.coeffs, r.coeffs))
}

pub(crate) fn linear_combination<F: Field>(
    polynomials: &[impl AsRef<[F]>],
    challenges: &[F],
) -> Option<Vec<F>> {
    polynomials
        .as_ref()
        .iter()
        .zip(challenges.iter())
        .map(|(p, &c)| &DensePolynomial::from_coefficients_slice(p.as_ref()) * c)
        .reduce(|x, y| x + y)?
        .coeffs
        .into()
}

pub(crate) fn gen_curve_powers_proj<G: ScalarMul + CurveGroup>(
    powers: &[G::ScalarField],
    base: G,
) -> Vec<G> {
    let window_size = FixedBase::get_mul_window_size(powers.len());
    let scalar_size = G::ScalarField::MODULUS_BIT_SIZE as usize;
    let g_table = FixedBase::get_window_table::<G>(scalar_size, window_size, base);
    FixedBase::msm::<G>(scalar_size, window_size, &g_table, powers)
}

pub(crate) fn gen_curve_powers<G: ScalarMul + CurveGroup>(
    powers: &[G::ScalarField],
    base: G,
) -> Vec<G::Affine> {
    G::normalize_batch(&gen_curve_powers_proj(powers, base))
}

pub(crate) fn get_field_size<F: Field + CanonicalSerialize>() -> usize {
    F::zero().serialized_size(Compress::Yes)
}

pub(crate) fn transcribe_points_and_evals<F: CanonicalSerialize>(
    transcript: &mut Transcript,
    points: &[F],
    evals: &[impl AsRef<[F]>],
    field_size_bytes: usize,
) -> Result<(), Error> {
    let n_points = points.len();
    let mut eval_bytes = vec![0u8; field_size_bytes * n_points * evals.len()];
    for (i, e) in evals.iter().enumerate() {
        if e.as_ref().len() != n_points {
            return Err(Error::EvalsIncorrectSize {
                poly: i,
                n: e.as_ref().len(),
                expected: n_points,
            });
        }
        for (j, p) in e.as_ref().iter().enumerate() {
            let start = (i * n_points + j) * field_size_bytes;
            p.serialize_compressed(&mut eval_bytes[start..start + field_size_bytes])?;
        }
    }
    transcript.append_message(b"open evals", &eval_bytes);
    let mut point_bytes = vec![0u8; field_size_bytes * n_points];
    for (i, p) in points.iter().enumerate() {
        p.serialize_compressed(&mut point_bytes[i * field_size_bytes..(i + 1) * field_size_bytes])?;
    }
    transcript.append_message(b"open points", &point_bytes);
    Ok(())
}

pub(crate) fn transcribe_generic<F: CanonicalSerialize>(
    transcript: &mut Transcript,
    label: &'static [u8],
    f: &F,
) -> Result<(), Error> {
    let elt_size = f.serialized_size(Compress::Yes);
    let mut buf = vec![0u8; elt_size];
    f.serialize_compressed(&mut buf)?;
    transcript.append_message(label, &buf);
    Ok(())
}

pub(crate) fn get_challenge<F: PrimeField>(
    transcript: &mut Transcript,
    label: &'static [u8],
    field_size_bytes: usize,
) -> F {
    let mut challenge_bytes = vec![0u8; field_size_bytes];
    transcript.challenge_bytes(label, &mut challenge_bytes);
    F::from_be_bytes_mod_order(&challenge_bytes)
}

pub(crate) fn check_opening_sizes<F>(
    evals: &[impl AsRef<[F]>],
    polys: &[impl AsRef<[F]>],
    points: &[F],
) -> Result<(), Error> {
    if evals.len() != polys.len() {
        return Err(Error::EvalsAndPolysDifferentSizes {
            n_eval_rows: evals.len(),
            n_polys: polys.len(),
        });
    }
    for e in evals {
        if e.as_ref().len() != points.len() {
            return Err(Error::EvalsAndPointsDifferentSizes {
                n_evals: e.as_ref().len(),
                n_points: points.len(),
            });
        }
    }
    Ok(())
}

pub(crate) fn check_verify_sizes<F, C>(
    commits: &[C],
    points: &[F],
    evals: &[impl AsRef<[F]>],
) -> Result<(), Error> {
    if evals.len() != commits.len() {
        return Err(Error::EvalsAndCommitsDifferentSizes {
            n_evals: evals.len(),
            n_commits: commits.len(),
        });
    }
    for e in evals {
        if e.as_ref().len() != points.len() {
            return Err(Error::EvalsAndPointsDifferentSizes {
                n_evals: e.as_ref().len(),
                n_points: points.len(),
            });
        }
    }
    Ok(())
}

#[macro_export]
macro_rules! cfg_iter {
    ($e: expr) => {{
        #[cfg(feature = "parallel")]
        let result = $e.par_iter().enumerate();

        #[cfg(not(feature = "parallel"))]
        let result = $e.iter().enumerate();

        result
    }};
}
