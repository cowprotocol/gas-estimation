use anyhow::{anyhow, Result};
use serde::Serialize;

/// EIP1559 gas price
#[derive(Debug, Default, Clone, Copy, PartialEq, PartialOrd, Serialize)]
pub struct GasPrice1559 {
    // Estimated base fee for the pending block (block currently being mined)
    pub base_fee_per_gas: f64,
    // Maximum gas price willing to pay for the transaction.
    pub max_fee_per_gas: f64,
    // Priority fee used to incentivize miners to include the tx in case of network congestion.
    pub max_priority_fee_per_gas: f64,
}

impl GasPrice1559 {
    // Estimate the effective gas price based on the current network conditions (base_fee_per_gas)
    // Beware that gas price for mined transaction could be different from estimated value in case of 1559 tx
    // (because base_fee_per_gas can change between estimation and mining the tx).
    pub fn effective_gas_price(&self) -> f64 {
        std::cmp::min_by(
            self.max_fee_per_gas,
            self.max_priority_fee_per_gas + self.base_fee_per_gas,
            |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
        )
    }

    // Validate against rules defined in https://eips.ethereum.org/EIPS/eip-1559
    // max_fee_per_gas >= max_priority_fee_per_gas
    // max_fee_per_gas >= base_fee_per_gas
    pub fn is_valid(&self) -> bool {
        self.max_fee_per_gas >= self.max_priority_fee_per_gas
            && self.max_fee_per_gas >= self.base_fee_per_gas
    }

    // Validate and build Result based on the validation result
    pub fn validate(self) -> Result<Self> {
        match self.is_valid() {
            true => Ok(self),
            false => Err(anyhow!("invalid gas price values: {:?}", self)),
        }
    }
    // Bump gas price by factor.
    pub fn bump(self, factor: f64) -> Self {
        Self {
            max_fee_per_gas: self.max_fee_per_gas * factor,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas * factor,
            ..self
        }
    }

    // Ceil gas price (since its defined as float).
    pub fn ceil(self) -> Self {
        Self {
            max_fee_per_gas: self.max_fee_per_gas.ceil(),
            max_priority_fee_per_gas: self.max_priority_fee_per_gas.ceil(),
            ..self
        }
    }

    // If current cap if higher then the input, set to input.
    pub fn limit_cap(self, cap: f64) -> Self {
        Self {
            max_fee_per_gas: self.max_fee_per_gas.min(cap),
            max_priority_fee_per_gas: self
                .max_priority_fee_per_gas
                .min(self.max_fee_per_gas.min(cap)), // enforce max_priority_fee_per_gas <= max_fee_per_gas
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::GasPrice1559;
    use assert_approx_eq::assert_approx_eq;

    #[test]
    fn bump_and_ceil() {
        let gas_price = GasPrice1559 {
            max_fee_per_gas: 2.0,
            max_priority_fee_per_gas: 3.0,
            ..Default::default()
        };

        let gas_price_bumped = GasPrice1559 {
            max_fee_per_gas: 2.25,
            max_priority_fee_per_gas: 3.375,
            ..Default::default()
        };

        let gas_price_bumped_and_ceiled = GasPrice1559 {
            max_fee_per_gas: 3.0,
            max_priority_fee_per_gas: 4.0,
            ..Default::default()
        };

        assert_eq!(gas_price.bump(1.125), gas_price_bumped);
        assert_eq!(gas_price.bump(1.125).ceil(), gas_price_bumped_and_ceiled);
    }

    #[test]
    fn limit_cap_only_max_fee_capped() {
        let gas_price = GasPrice1559 {
            max_fee_per_gas: 5.0,
            max_priority_fee_per_gas: 3.0,
            ..Default::default()
        };

        let gas_price_capped = GasPrice1559 {
            max_fee_per_gas: 4.0,
            max_priority_fee_per_gas: 3.0,
            ..Default::default()
        };

        assert_eq!(gas_price.limit_cap(4.0), gas_price_capped);
    }

    #[test]
    fn limit_cap_max_fee_and_max_priority_capped() {
        let gas_price = GasPrice1559 {
            max_fee_per_gas: 5.0,
            max_priority_fee_per_gas: 3.0,
            ..Default::default()
        };

        let gas_price_capped = GasPrice1559 {
            max_fee_per_gas: 2.0,
            max_priority_fee_per_gas: 2.0,
            ..Default::default()
        };

        assert_eq!(gas_price.limit_cap(2.0), gas_price_capped);
    }

    #[test]
    fn estimate_eip1559() {
        assert_approx_eq!(
            GasPrice1559 {
                max_fee_per_gas: 10.0,
                max_priority_fee_per_gas: 5.0,
                base_fee_per_gas: 2.0
            }
            .effective_gas_price(),
            7.0
        );

        assert_approx_eq!(
            GasPrice1559 {
                max_fee_per_gas: 10.0,
                max_priority_fee_per_gas: 8.0,
                base_fee_per_gas: 2.0
            }
            .effective_gas_price(),
            10.0
        );

        assert_approx_eq!(
            GasPrice1559 {
                max_fee_per_gas: 10.0,
                max_priority_fee_per_gas: 10.0,
                base_fee_per_gas: 2.0
            }
            .effective_gas_price(),
            10.0
        );
    }
}
