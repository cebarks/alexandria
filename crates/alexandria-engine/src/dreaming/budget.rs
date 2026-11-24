//! Per-job row allowance accounting for a single scheduler/CLI pass.
//!
//! Reservations are charged before work starts. Dropping a reservation leaves its charge intact;
//! only a verified successful result can return unused capacity to the pass.

#[derive(Debug)]
pub struct RowBudget {
    remaining: usize,
}

#[derive(Debug)]
pub struct Reservation {
    rows: usize,
}

impl Reservation {
    /// The rows this reservation carries: what the dispatcher may spend before the pass's
    /// accounting sees the outcome.
    pub fn rows(&self) -> usize {
        self.rows
    }
}

impl RowBudget {
    pub fn new(rows: usize) -> Self {
        Self { remaining: rows }
    }

    pub fn remaining(&self) -> usize {
        self.remaining
    }

    pub fn reserve(&mut self, requested: usize) -> Option<Reservation> {
        if requested == 0 || self.remaining == 0 {
            return None;
        }
        let rows = requested.min(self.remaining);
        self.remaining -= rows;
        Some(Reservation { rows })
    }

    pub fn refund_verified(
        &mut self,
        reservation: Reservation,
        used: usize,
    ) -> Result<(), &'static str> {
        let unused = reservation
            .rows
            .checked_sub(used)
            .ok_or("reported usage exceeds reserved capacity")?;
        self.remaining += unused;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Missing reservation charging would let successive partitions exceed the pass-wide cap.
    #[test]
    fn reservation_is_charged_before_dispatch() {
        let mut budget = RowBudget::new(5);
        let _reservation = budget.reserve(3).expect("turn allowance available");
        assert_eq!(
            budget.remaining(),
            2,
            "dispatch must charge the whole reservation"
        );
    }

    // An unused successful share must remain usable by other partitions in this pass.
    #[test]
    fn verified_success_refunds_only_unused_capacity() {
        let mut budget = RowBudget::new(5);
        let reservation = budget.reserve(3).expect("turn allowance available");
        budget.refund_verified(reservation, 1).unwrap();
        assert_eq!(budget.remaining(), 4, "one verified unit remains charged");
    }

    // An errored or cancelled partition may have committed its full allowance before failing.
    #[test]
    fn unknown_partial_outcome_cannot_refund_or_dispatch_another_partition() {
        let mut budget = RowBudget::new(4);
        {
            let _unknown_outcome = budget.reserve(4).expect("full allowance available");
        }
        assert_eq!(budget.remaining(), 0, "unknown outcome never refunds");
        assert!(
            budget.reserve(1).is_none(),
            "no capped turn after exhaustion"
        );
    }

    // Reject an invalid success report without minting capacity or undoing the prior charge.
    #[test]
    fn verified_refund_cannot_exceed_the_reservation() {
        let mut budget = RowBudget::new(3);
        let reservation = budget.reserve(2).unwrap();
        assert!(budget.refund_verified(reservation, 3).is_err());
        assert_eq!(budget.remaining(), 1);
    }
}
