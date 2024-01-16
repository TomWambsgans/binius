// Copyright 2023 Ulvetanna Inc.

use super::{error::Error, multilinear::MultilinearPoly};
use crate::field::{
	get_packed_slice, iter_packed_slice, set_packed_slice, ExtensionField, Field, PackedField,
};
use itertools::Either;
use p3_util::log2_strict_usize;
use std::{borrow::Cow, fmt::Debug};

/// A multilinear polynomial represented by its evaluations over the boolean hypercube.
///
/// This polynomial can also be viewed as the multilinear extension of the slice of hypercube
/// evaluations. The evaluation data may be either a borrowed or owned slice.
///
/// The packed field width must be a power of two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultilinearExtension<'a, P: PackedField> {
	// The number of variables
	mu: usize,
	// The evaluations of the polynomial over the boolean hypercube, in lexicographic order
	evals: Cow<'a, [P]>,
}

impl<P: PackedField> MultilinearExtension<'static, P> {
	pub fn zeros(n_vars: usize) -> Result<Self, Error> {
		assert!(P::WIDTH.is_power_of_two());
		if n_vars < log2_strict_usize(P::WIDTH) {
			return Err(Error::ArgumentRangeError {
				arg: "n_vars".to_string(),
				range: log2_strict_usize(P::WIDTH)..32,
			});
		}

		Ok(MultilinearExtension {
			mu: n_vars,
			evals: Cow::Owned(vec![P::default(); 1 << (n_vars - log2(P::WIDTH))]),
		})
	}

	pub fn from_values(v: Vec<P>) -> Result<Self, Error> {
		if !v.len().is_power_of_two() {
			return Err(Error::PowerOfTwoLengthRequired);
		}
		let mu = log2(v.len() * P::WIDTH);
		Ok(Self {
			mu,
			evals: Cow::Owned(v),
		})
	}
}

impl<'a, P: PackedField> MultilinearExtension<'a, P> {
	pub fn from_values_slice(v: &'a [P]) -> Result<Self, Error> {
		if !v.len().is_power_of_two() {
			return Err(Error::PowerOfTwoLengthRequired);
		}
		let mu = log2(v.len() * P::WIDTH);
		Ok(Self {
			mu,
			evals: Cow::Borrowed(v),
		})
	}

	pub fn n_vars(&self) -> usize {
		self.mu
	}

	pub fn size(&self) -> usize {
		1 << self.mu
	}

	pub fn evals(&self) -> &[P] {
		self.evals.as_ref()
	}

	pub fn borrow_copy(&self) -> MultilinearExtension<P> {
		MultilinearExtension {
			mu: self.mu,
			evals: Cow::Borrowed(self.evals()),
		}
	}

	/// Get the evaluations of the polynomial on a subcube of the hypercube of size equal to the
	/// packing width.
	///
	/// # Arguments
	///
	/// * `index` - The index of the subcube
	pub fn packed_evaluate_on_hypercube(&self, index: usize) -> Result<P, Error> {
		self.evals()
			.get(index)
			.ok_or(Error::HypercubeIndexOutOfRange { index })
			.copied()
	}

	pub fn evaluate<FE>(&self, q: &[FE]) -> Result<FE, Error>
	where
		FE: ExtensionField<P::Scalar>,
	{
		if self.mu != q.len() {
			return Err(Error::IncorrectQuerySize { expected: self.mu });
		}
		let basis_eval = expand_query(q)?;
		let result =
			inner_product_unchecked(basis_eval.into_iter(), iter_packed_slice(&self.evals));
		Ok(result)
	}

	pub fn batch_evaluate<FE: ExtensionField<P::Scalar>>(
		polys: impl Iterator<Item = Self> + 'a,
		q: &[FE],
	) -> impl Iterator<Item = Result<FE, Error>> + 'a {
		let n_vars = q.len();
		let basis_eval = expand_query(q);

		polys.map(move |poly| {
			let basis_eval = basis_eval.as_ref().map_err(Clone::clone)?;

			if poly.mu != n_vars {
				return Err(Error::IncorrectQuerySize { expected: poly.mu });
			}

			let result =
				inner_product_unchecked(basis_eval.iter().cloned(), iter_packed_slice(&poly.evals));

			Ok(result)
		})
	}

	/// Partially evaluate the polynomial with assignment to the high-indexed variables.
	///
	/// The polynomial is multilinear with $\mu$ variables, $p(X_0, ..., X_{\mu - 1}$. Given a query
	/// vector of length $k$ representing $(z_{\mu - k + 1}, ..., z_{\mu - 1})$, this returns the
	/// multilinear polynomial with $\mu - k$ variables,
	/// $p(X_0, ..., X_{\mu - k}, z_{\mu - k + 1}, ..., z_{\mu - 1})$.
	///
	/// REQUIRES: the size of the resulting polynomial must have a length which is a multiple of
	/// PE::WIDTH, i.e. 2^(\mu - k) \geq PE::WIDTH, since WIDTH is power of two
	pub fn evaluate_partial_high<PE>(
		&self,
		q: &[PE::Scalar],
	) -> Result<MultilinearExtension<'static, PE>, Error>
	where
		PE: PackedField,
		PE::Scalar: ExtensionField<P::Scalar>,
	{
		if self.mu < q.len() {
			return Err(Error::IncorrectQuerySize { expected: self.mu });
		}
		if (1 << (self.mu - q.len())) < PE::WIDTH {
			return Err(Error::IncorrectQuerySize { expected: self.mu });
		}

		// TODO: Optimize this by packing expanded query and using packed arithmetic.
		let basis_eval = expand_query(q)?;

		let mut result_evals = vec![PE::default(); (1 << (self.mu - q.len())) / PE::WIDTH];
		self.iter_subpolynomials_high(self.mu - q.len())?
			.zip(basis_eval)
			.for_each(|(subpoly, basis_eval)| {
				for (i, subpoly_eval_i) in iter_packed_slice(subpoly.evals()).enumerate() {
					let mut value = get_packed_slice(&result_evals, i);
					value += basis_eval * subpoly_eval_i;
					set_packed_slice(&mut result_evals, i, value);
				}
			});

		MultilinearExtension::from_values(result_evals)
	}

	pub fn iter_subpolynomials_high(
		&self,
		n_vars: usize,
	) -> Result<impl Iterator<Item = MultilinearExtension<P>>, Error> {
		let log_width = log2(P::WIDTH);
		if n_vars < log_width || n_vars > self.mu {
			return Err(Error::ArgumentRangeError {
				arg: "n_vars".into(),
				range: log_width..self.mu,
			});
		}

		let iter = self
			.evals
			.chunks_exact(1 << (n_vars - log_width))
			.map(move |evals| MultilinearExtension {
				mu: n_vars,
				evals: Cow::Borrowed(evals),
			});
		Ok(iter)
	}

	/// Partially evaluate the polynomial with assignment to the low-indexed variables.
	///
	/// The polynomial is multilinear with $\mu$ variables, $p(X_0, ..., X_{\mu-1}$. Given a query
	/// vector of length $k$ representing $(z_0, ..., z_{k-1})$, this returns the
	/// multilinear polynomial with $\mu - k$ variables,
	/// $p(z_0, ..., z_{k-1}, X_k, ..., X_{\mu - 1})$.
	///
	/// REQUIRES: the size of the resulting polynomial must have a length which is a multiple of
	/// P::WIDTH, i.e. 2^(\mu - k) \geq P::WIDTH, since WIDTH is power of two
	pub fn evaluate_partial_low<PE>(
		&self,
		q: &[PE::Scalar],
	) -> Result<MultilinearExtension<'static, PE>, Error>
	where
		PE: PackedField,
		PE::Scalar: ExtensionField<P::Scalar>,
	{
		if self.mu < q.len() {
			return Err(Error::IncorrectQuerySize { expected: self.mu });
		}
		let mut result = MultilinearExtension::zeros(self.mu - q.len())?;
		self.evaluate_partial_low_into(q, &mut result)?;
		Ok(result)
	}

	/// Partially evaluate the polynomial with assignment to the low-indexed variables.
	///
	/// The polynomial is multilinear with $\mu$ variables, $p(X_0, ..., X_{\mu-1}$. Given a query
	/// vector of length $k$ representing $(z_0, ..., z_{k-1})$, this returns the
	/// multilinear polynomial with $\mu - k$ variables,
	/// $p(z_0, ..., z_{k-1}, X_k, ..., X_{\mu - 1})$.
	///
	/// REQUIRES: the size of the resulting polynomial must have a length which is a multiple of
	/// P::WIDTH, i.e. 2^(\mu - k) \geq P::WIDTH, since WIDTH is power of two
	pub fn evaluate_partial_low_into<PE>(
		&self,
		q: &[PE::Scalar],
		out: &mut MultilinearExtension<'static, PE>,
	) -> Result<(), Error>
	where
		PE: PackedField,
		PE::Scalar: ExtensionField<P::Scalar>,
	{
		if self.mu < q.len() {
			return Err(Error::IncorrectQuerySize { expected: self.mu });
		}
		if out.n_vars() != self.mu - q.len() {
			return Err(Error::IncorrectOutputPolynomialSize {
				expected: self.mu - q.len(),
			});
		}

		let basis_evals = expand_query(q)?;

		let packed_result_evals = out.evals.to_mut();
		for (i, packed_result_eval) in packed_result_evals.iter_mut().enumerate() {
			(0..P::WIDTH).for_each(|j| {
				let mut result_eval = PE::Scalar::ZERO;
				for (k, &basis_eval_k) in basis_evals.iter().enumerate() {
					let old_slice_idx = (i * P::WIDTH + j) << q.len() | k;
					let old_eval = get_packed_slice(&self.evals, old_slice_idx);
					result_eval += basis_eval_k * old_eval;
				}
				packed_result_eval.set(j, result_eval);
			});
		}
		Ok(())
	}

	#[inline]
	fn iter_subcube_scalars(
		&self,
		n_vars: usize,
		index: usize,
	) -> Result<impl Iterator<Item = P::Scalar> + '_, Error> {
		if n_vars > self.n_vars() {
			return Err(Error::ArgumentRangeError {
				arg: "n_vars".into(),
				range: 0..self.n_vars() + 1,
			});
		}

		let max_index = 1 << (self.n_vars() - n_vars);
		if index >= max_index {
			return Err(Error::ArgumentRangeError {
				arg: "index".into(),
				range: 0..max_index,
			});
		}

		let log_width = log2_strict_usize(P::WIDTH);
		let iter = if n_vars < log_width {
			Either::Left(
				self.evals[(index << n_vars) / P::WIDTH]
					.iter()
					.take(1 << n_vars),
			)
		} else {
			Either::Right(iter_packed_slice(
				&self.evals[((index << n_vars) / P::WIDTH)..(((index + 1) << n_vars) / P::WIDTH)],
			))
		};
		Ok(iter)
	}
}

impl<'a, P, PE> MultilinearPoly<PE> for MultilinearExtension<'a, P>
where
	P: PackedField + Debug,
	PE: PackedField,
	PE::Scalar: ExtensionField<P::Scalar>,
{
	fn n_vars(&self) -> usize {
		self.mu
	}

	fn evaluate_on_hypercube(&self, index: usize) -> Result<PE::Scalar, Error> {
		let subcube_eval = self.packed_evaluate_on_hypercube(index / P::WIDTH)?;
		Ok(subcube_eval.get(index % P::WIDTH).into())
	}

	fn evaluate(&self, q: &[PE::Scalar]) -> Result<PE::Scalar, Error> {
		self.evaluate(q)
	}

	fn evaluate_partial_low(
		&self,
		q: &[PE::Scalar],
	) -> Result<MultilinearExtension<'static, PE>, Error> {
		self.evaluate_partial_low(q)
	}

	fn inner_prod_subcube(&self, index: usize, expanded_query: &[PE]) -> Result<PE::Scalar, Error> {
		if !expanded_query.len().is_power_of_two() {
			return Err(Error::PowerOfTwoLengthRequired);
		}
		let q_vars = log2_strict_usize(expanded_query.len());

		let ret = inner_product_unchecked(
			iter_packed_slice(expanded_query),
			self.iter_subcube_scalars(q_vars, index)?,
		);
		Ok(ret)
	}

	fn subcube_evals(&self, vars: usize, index: usize, dst: &mut [PE]) -> Result<(), Error> {
		if vars > self.n_vars() {
			return Err(Error::ArgumentRangeError {
				arg: "vars".to_string(),
				range: 0..self.n_vars() + 1,
			});
		}
		// TODO: Handle the case when 1 << vars < PE::WIDTH
		if dst.len() * PE::WIDTH != 1 << vars {
			return Err(Error::ArgumentRangeError {
				arg: "dst.len()".to_string(),
				range: (1 << vars) / PE::WIDTH..(1 << vars) / PE::WIDTH + 1,
			});
		}
		if index >= 1 << (self.n_vars() - vars) {
			return Err(Error::ArgumentRangeError {
				arg: "index".to_string(),
				range: 0..(1 << (self.n_vars() - vars)),
			});
		}

		let evals = &self.evals()[(index << vars) / PE::WIDTH..((index + 1) << vars) / PE::WIDTH];
		for i in 0..1 << vars {
			set_packed_slice(dst, i, get_packed_slice(evals, i).into());
		}
		Ok(())
	}
}

/// Given n_vars, and a vector r of length n_vars, returns the multilinear polynomial
/// corresponding to the MLE of eq(X, Y) partially evaluated at r, i.e. eq_r(X) := eq(X, r)
/// eq_r(X) = \prod_{i=0}^{n_vars - 1} (X_i r_i + (1 - X_i)(1-r_i))
///
/// Recall multilinear polynomial eq(X, Y) = \prod_{i=0}^{n_vars - 1} (X_iY_i + (1 - X_i)(1-Y_i)).
/// This has the property that if X = Y then eq(X, Y) = 1, and if X != Y then eq(X, Y) = 0, over boolean hypercube domain.
pub fn eq_ind_partial_eval<F: Field>(
	n_vars: usize,
	r: &[F],
) -> Result<MultilinearExtension<'static, F>, Error> {
	if r.len() != n_vars {
		return Err(Error::IncorrectQuerySize { expected: n_vars });
	}
	let values = expand_query(r)?;
	MultilinearExtension::from_values(values)
}

/// Expand the tensor product of the query values.
///
/// [`query`] is a sequence of field elements $z_0, ..., z_{k-1}$. The expansion is given by the
/// tensor product $(1 - z_0, z0) \bigotimes \ldots \bigotimes (1 - z_k, z_k)$, which has length
/// $2^k$.
///
/// This naive implementation runs in O(2^k) time and O(2^k) space.
fn expand_query<F: Field>(query: &[F]) -> Result<Vec<F>, Error> {
	let query_len: u32 = query
		.len()
		.try_into()
		.map_err(|_| Error::TooManyVariables)?;
	let size = 2usize
		.checked_pow(query_len)
		.ok_or(Error::TooManyVariables)?;

	let mut result = vec![F::ZERO; size];
	result[0] = F::ONE;
	for (i, v) in query.iter().enumerate() {
		let mid = 1 << i;
		result.copy_within(0..mid, mid);
		for j in 0..mid {
			let prod = result[j] * *v;
			result[j] -= prod;
			result[mid + j] = prod;
		}
	}

	Ok(result)
}

/// Expand the tensor product of the query values.
///
/// [`query`] is a sequence of field elements $z_0, ..., z_{k-1}$.
///
/// This naive implementation runs in O(k 2^k) time and O(1) space.
#[allow(dead_code)]
fn expand_query_naive<F: Field>(query: &[F]) -> Result<Vec<F>, Error> {
	let query_len: u32 = query
		.len()
		.try_into()
		.map_err(|_| Error::TooManyVariables)?;
	let size = 2usize
		.checked_pow(query_len)
		.ok_or(Error::TooManyVariables)?;

	let result = (0..size).map(|i| eval_basis(query, i)).collect();
	Ok(result)
}

/// Evaluates the Lagrange basis polynomial over the boolean hypercube at a queried point.
#[allow(dead_code)]
fn eval_basis<F: Field>(query: &[F], i: usize) -> F {
	query
		.iter()
		.enumerate()
		.map(|(j, &v)| if i & (1 << j) == 0 { F::ONE - v } else { v })
		.product()
}

/// Computes the inner product of two vectors without checking that the lengths are equal
fn inner_product_unchecked<F, FE>(a: impl Iterator<Item = FE>, b: impl Iterator<Item = F>) -> FE
where
	F: Field,
	FE: ExtensionField<F>,
{
	a.zip(b).map(|(a_i, b_i)| a_i * b_i).sum::<FE>()
}

fn log2(v: usize) -> usize {
	63 - (v as u64).leading_zeros() as usize
}

#[cfg(test)]
mod tests {
	use super::*;
	use assert_matches::assert_matches;
	use rand::{rngs::StdRng, SeedableRng};
	use std::iter::repeat_with;

	use crate::field::{unpack_scalars_mut, BinaryField16b as F};

	#[test]
	fn test_expand_query_impls_consistent() {
		let mut rng = StdRng::seed_from_u64(0);
		let q = repeat_with(|| Field::random(&mut rng))
			.take(8)
			.collect::<Vec<F>>();
		let result1 = expand_query(&q).unwrap();
		let result2 = expand_query_naive(&q).unwrap();
		assert_eq!(result1, result2);
	}

	#[test]
	fn test_evaluate_on_hypercube() {
		let mut values = vec![F::ZERO; 64];
		unpack_scalars_mut(&mut values)
			.iter_mut()
			.enumerate()
			.for_each(|(i, val)| *val = F::new(i as u16));

		let poly = MultilinearExtension::from_values(values).unwrap();
		for i in 0..64 {
			let q = (0..6)
				.map(|j| if (i >> j) & 1 != 0 { F::ONE } else { F::ZERO })
				.collect::<Vec<_>>();
			let result = poly.evaluate(&q).unwrap();
			assert_eq!(result, F::new(i));
		}
	}

	#[test]
	fn test_iter_subpolynomials() {
		let mut rng = StdRng::seed_from_u64(0);
		let values = repeat_with(|| Field::random(&mut rng))
			.take(8)
			.collect::<Vec<F>>();

		let poly = MultilinearExtension::from_values_slice(&values).unwrap();

		let mut iter = poly.iter_subpolynomials_high(2).unwrap();

		let expected_poly0 = MultilinearExtension::from_values_slice(&values[0..4]).unwrap();
		assert_eq!(iter.next().unwrap(), expected_poly0);

		let expected_poly1 = MultilinearExtension::from_values_slice(&values[4..8]).unwrap();
		assert_eq!(iter.next().unwrap(), expected_poly1);

		assert_matches!(iter.next(), None);
	}

	fn evaluate_split<P>(
		poly: MultilinearExtension<P>,
		q: &[P::Scalar],
		splits: &[usize],
	) -> P::Scalar
	where
		P: PackedField + 'static,
	{
		assert_eq!(splits.iter().sum::<usize>(), poly.n_vars());

		let mut partial_result = poly.borrow_copy();
		let mut index = q.len();
		for split_vars in splits[0..splits.len() - 1].iter() {
			partial_result = partial_result
				.evaluate_partial_high(&q[index - split_vars..index])
				.unwrap();
			index -= split_vars;
		}

		partial_result.evaluate(&q[..index]).unwrap()
	}

	#[test]
	fn test_evaluate_split_is_correct() {
		let mut rng = StdRng::seed_from_u64(0);
		let evals = repeat_with(|| Field::random(&mut rng))
			.take(256)
			.collect::<Vec<F>>();
		let poly = MultilinearExtension::from_values(evals).unwrap();
		let q = repeat_with(|| Field::random(&mut rng))
			.take(8)
			.collect::<Vec<F>>();
		let result1 = poly.evaluate(&q).unwrap();
		let result2 = evaluate_split(poly, &q, &[2, 3, 3]);
		assert_eq!(result1, result2);
	}

	#[test]
	fn test_batch_evaluate() {
		let mut rng = StdRng::seed_from_u64(0);
		let poly1 = MultilinearExtension::from_values(
			repeat_with(|| <F as Field>::random(&mut rng))
				.take(256)
				.collect(),
		)
		.unwrap();
		let poly2 = MultilinearExtension::from_values(
			repeat_with(|| <F as Field>::random(&mut rng))
				.take(256)
				.collect(),
		)
		.unwrap();
		let poly3 = MultilinearExtension::from_values(
			repeat_with(|| <F as Field>::random(&mut rng))
				.take(128)
				.collect(),
		)
		.unwrap();

		let q = repeat_with(|| <F as Field>::random(&mut rng))
			.take(8)
			.collect::<Vec<F>>();

		let expected_eval1 = poly1.evaluate(&q).unwrap();
		let expected_eval2 = poly2.evaluate(&q).unwrap();

		let mut eval_iter =
			MultilinearExtension::batch_evaluate(vec![poly1, poly2, poly3].into_iter(), &q);
		assert_eq!(eval_iter.next().unwrap().unwrap(), expected_eval1);
		assert_eq!(eval_iter.next().unwrap().unwrap(), expected_eval2);
		assert_matches!(eval_iter.next(), Some(Err(Error::IncorrectQuerySize { .. })));
		assert_matches!(eval_iter.next(), None);
	}
}
