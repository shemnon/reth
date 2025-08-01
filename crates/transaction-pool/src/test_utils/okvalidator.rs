use std::marker::PhantomData;

use crate::{
    validate::ValidTransaction, EthPooledTransaction, PoolTransaction, TransactionOrigin,
    TransactionValidationOutcome, TransactionValidator,
};

/// A transaction validator that determines all transactions to be valid.
#[derive(Debug)]
#[non_exhaustive]
pub struct OkValidator<T = EthPooledTransaction> {
    _phantom: PhantomData<T>,
    /// Whether to mark transactions as propagatable.
    propagate: bool,
}

impl<T> OkValidator<T> {
    /// Determines whether transactions should be allowed to be propagated
    pub const fn set_propagate_transactions(mut self, propagate: bool) -> Self {
        self.propagate = propagate;
        self
    }
}

impl<T> Default for OkValidator<T> {
    fn default() -> Self {
        Self { _phantom: Default::default(), propagate: false }
    }
}

impl<T> TransactionValidator for OkValidator<T>
where
    T: PoolTransaction,
{
    type Transaction = T;

    async fn validate_transaction(
        &self,
        _origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        // Always return valid
        let authorities = transaction.authorization_list().map(|auths| {
            auths.iter().flat_map(|auth| auth.recover_authority()).collect::<Vec<_>>()
        });
        TransactionValidationOutcome::Valid {
            balance: *transaction.cost(),
            state_nonce: transaction.nonce(),
            bytecode_hash: None,
            transaction: ValidTransaction::Valid(transaction),
            propagate: self.propagate,
            authorities,
        }
    }
}
