//! # BDFG Method 2
use crate::{
    check_opening_sizes, check_verify_sizes,
    lagrange::LagrangeInterpContext,
    traits::{Committer, PolyMultiProofNoPrecomp},
};
use ark_ff::One;
use ark_poly::{univariate::DensePolynomial, DenseUVPolynomial, Polynomial};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use ark_std::{vec, vec::Vec, UniformRand};
use core::ops::{Div, Mul, Sub};
use merlin::Transcript;

use ark_ec::{pairing::Pairing, AffineRepr, CurveGroup};
use ark_std::rand::RngCore;

use crate::{
    get_challenge, get_field_size, transcribe_generic, transcribe_points_and_evals, Commitment,
};

use crate::{
    gen_curve_powers, gen_powers, linear_combination, poly_div_q_r, vanishing_polynomial, Error,
};

pub mod precompute;

/// A method 2 proof scheme with no precomputation of lagrange polynomials
#[derive(Clone, Debug)]
pub struct M2NoPrecomp<E: Pairing> {
    /// The given powers tau in G1
    pub powers_of_g1: Vec<E::G1Affine>,
    /// The G2 generator
    pub g2: E::G2Affine,
    /// The G2 generator multiplied by tau
    pub g2x: E::G2Affine,
}

impl<E: Pairing> M2NoPrecomp<E> {
    /// Make a new scheme from the given powers of tau and generators in affine form
    pub fn new_from_affine(
        powers_of_g1: Vec<E::G1Affine>,
        g2: E::G2Affine,
        g2x: E::G2Affine,
    ) -> Self {
        Self {
            powers_of_g1,
            g2,
            g2x,
        }
    }

    /// Make a new scheme from the given powers of tau and generators in projective form
    pub fn new_from_powers(powers_of_g1: &[E::G1], g2: &E::G2, g2x: &E::G2) -> Self {
        Self::new_from_affine(
            powers_of_g1.iter().map(|s| s.into_affine()).collect(),
            g2.into_affine(),
            g2x.into_affine(),
        )
    }

    /// Generate a new scheme with random generators and powers of tau
    pub fn new(max_coeffs: usize, rng: &mut impl RngCore) -> Self {
        let x = E::ScalarField::rand(rng);
        let g1 = E::G1::rand(rng);
        let g2 = E::G2::rand(rng);
        Self::new_from_scalar(x, g1, g2, max_coeffs)
    }

    /// Generate a new scheme from a known secret scalar
    pub fn new_from_scalar(x: E::ScalarField, g1: E::G1, g2: E::G2, max_coeffs: usize) -> Self {
        let x_powers = gen_powers(x, max_coeffs);
        let powers_of_g1 = gen_curve_powers::<E::G1>(x_powers.as_ref(), g1);
        let g2x = (g2 * x).into_affine();

        Self::new_from_affine(powers_of_g1, g2.into_affine(), g2x)
    }
}

/// A proof for method 2
#[derive(Clone, Debug, CanonicalSerialize, CanonicalDeserialize)]
pub struct Proof<E: Pairing>(pub E::G1Affine, pub E::G1Affine);

impl<E: Pairing> M2NoPrecomp<E> {
    fn open_with_vanishing_poly(
        &self,
        transcript: &mut Transcript,
        evals: &[impl AsRef<[E::ScalarField]>],
        polys: &[impl AsRef<[E::ScalarField]>],
        points: &[E::ScalarField],
        vp: &DensePolynomial<E::ScalarField>,
    ) -> Result<Proof<E>, Error> {
        check_opening_sizes(evals, polys, points)?;

        let field_size_bytes = get_field_size::<E::ScalarField>();
        transcribe_points_and_evals(transcript, points, evals, field_size_bytes)?;

        let gamma = get_challenge(transcript, b"open gamma", field_size_bytes);

        let gammas = gen_powers::<E::ScalarField>(gamma, self.powers_of_g1.len());
        let gamma_fis = linear_combination::<E::ScalarField>(polys, &gammas)
            .ok_or(Error::NoPolynomialsGiven)?;
        let gamma_fis_poly = DensePolynomial::from_coefficients_vec(gamma_fis);

        let (h, gamma_ris_over_zs) = poly_div_q_r((&gamma_fis_poly).into(), (vp).into())?;

        let w_1 = crate::curve_msm::<E::G1>(&self.powers_of_g1, &h)?.into_affine();

        transcribe_generic(transcript, b"open W1", &w_1)?;
        let chal_z = get_challenge(transcript, b"open z", field_size_bytes);

        let gamma_ri_z = DensePolynomial::from_coefficients_vec(gamma_ris_over_zs)
            .mul(vp)
            .evaluate(&chal_z);

        let f_z = gamma_fis_poly.sub(&DensePolynomial::from_coefficients_vec(vec![gamma_ri_z])); // XXX
        let l = f_z.sub(&DensePolynomial::from_coefficients_vec(h).mul(vp.evaluate(&chal_z)));

        let x_minus_z =
            DensePolynomial::from_coefficients_vec(vec![-chal_z, E::ScalarField::one()]);
        let l_quotient = l.div(&x_minus_z);

        let w_2 = crate::curve_msm::<E::G1>(&self.powers_of_g1, &l_quotient)?.into_affine();
        Ok(Proof(w_1, w_2))
    }

    fn verify_with_lag_ctx_vanishing_poly(
        &self,
        transcript: &mut Transcript,
        commits: &[Commitment<E>],
        points: &[E::ScalarField],
        evals: &[impl AsRef<[E::ScalarField]>],
        proof: &Proof<E>,
        lag_ctx: &LagrangeInterpContext<E::ScalarField>,
        vp: &DensePolynomial<E::ScalarField>,
    ) -> Result<bool, Error> {
        check_verify_sizes(commits, points, evals)?;

        let field_size_bytes = get_field_size::<E::ScalarField>();
        transcribe_points_and_evals(transcript, points, evals, field_size_bytes)?;

        let gamma = get_challenge(transcript, b"open gamma", field_size_bytes);
        transcribe_generic(transcript, b"open W1", &proof.0)?;
        let chal_z = get_challenge(transcript, b"open z", field_size_bytes);

        let zeros_z = vp.evaluate(&chal_z);

        // Get the r_i polynomials with lagrange interp. These could be precomputed.
        let gammas = gen_powers(gamma, evals.len());
        // Get the gamma^i r_i polynomials with lagrange interp. This does both the lagrange interp
        // and the gamma mul in one step so we can just lagrange interp once.
        let gamma_ris = lag_ctx.lagrange_interp_linear_combo(evals, &gammas)?.coeffs;
        let gamma_ris_z = DensePolynomial::from_coefficients_vec(gamma_ris).evaluate(&chal_z);
        let gamma_ris_z_pt = self.powers_of_g1[0].mul(gamma_ris_z);

        // Then do a single msm of the gammas and commitments
        let cms = commits.iter().map(|i| i.0).collect::<Vec<_>>();
        let gamma_cm_pt = crate::curve_msm::<E::G1>(&cms, gammas.as_ref())?;

        let f = gamma_cm_pt - gamma_ris_z_pt - proof.0.mul(zeros_z);

        let x_minus_z = self.g2x.into_group() - self.g2.into_group().mul(&chal_z);
        Ok(E::pairing(f, self.g2) == E::pairing(proof.1, x_minus_z))
    }
}

impl<E: Pairing> Committer<E> for M2NoPrecomp<E> {
    fn commit(&self, poly: impl AsRef<[E::ScalarField]>) -> Result<Commitment<E>, Error> {
        let res = crate::curve_msm::<E::G1>(&self.powers_of_g1, poly.as_ref())?;
        Ok(Commitment(res.into_affine()))
    }
}

impl<E: Pairing> PolyMultiProofNoPrecomp<E> for M2NoPrecomp<E> {
    type Proof = Proof<E>;

    fn open(
        &self,
        transcript: &mut Transcript,
        evals: &[impl AsRef<[E::ScalarField]>],
        polys: &[impl AsRef<[E::ScalarField]>],
        points: &[E::ScalarField],
    ) -> Result<Proof<E>, Error> {
        let vp = vanishing_polynomial(points.as_ref());
        self.open_with_vanishing_poly(transcript, evals, polys, points, &vp)
    }

    fn verify(
        &self,
        transcript: &mut Transcript,
        commits: &[Commitment<E>],
        points: &[E::ScalarField],
        evals: &[impl AsRef<[E::ScalarField]>],
        proof: &Proof<E>,
    ) -> Result<bool, Error> {
        let vp = vanishing_polynomial(points);
        let lag_ctx = LagrangeInterpContext::new_from_points(points)?;
        self.verify_with_lag_ctx_vanishing_poly(
            transcript, commits, points, evals, proof, &lag_ctx, &vp,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::M2NoPrecomp;
    use crate::{
        test_rng,
        testing::{test_basic_no_precomp, test_size_errors},
    };
    use ark_bls12_381::Bls12_381;

    #[test]
    fn test_basic_open_works() {
        let s = M2NoPrecomp::<Bls12_381>::new(256, &mut test_rng());
        test_basic_no_precomp(&s);
        test_size_errors(&s);
    }
}
