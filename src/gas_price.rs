use ethcontract::GasPrice;
/// Gas price received from the gas price estimators.
use serde::Serialize;

#[derive(Debug, Default, Clone, Copy, PartialEq, PartialOrd, Serialize)]
/// Main gas price structure.
/// Provide estimated gas prices for both legacy and eip1559 transactions.
pub struct EstimatedGasPrice {
    // Estimated gas price for legacy type of transactions.
    pub legacy: f64,
    // Estimated gas price for 1559 type of transactions. Optional because not all gas estimators support 1559.
    pub eip1559: Option<GasPrice1559>,
}

impl EstimatedGasPrice {
    // Estimate the effective gas price based on the current network conditions (base_fee_per_gas)
    // Beware that gas price for mined transaction could be different from estimated value in case of 1559 tx
    // (because base_fee_per_gas can change between estimation and mining the tx).
    pub fn effective_gas_price(&self) -> f64 {
        if let Some(gas_price) = &self.eip1559 {
            std::cmp::min_by(
                gas_price.max_fee_per_gas,
                gas_price.max_priority_fee_per_gas + gas_price.base_fee_per_gas,
                |a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal),
            )
        } else {
            self.legacy
        }
    }

    // Maximum gas price willing to pay for the transaction.
    pub fn cap(&self) -> f64 {
        if let Some(gas_price) = &self.eip1559 {
            gas_price.max_fee_per_gas
        } else {
            self.legacy
        }
    }

    // Maximum tip willing to pay to miners for transaction.
    pub fn tip(&self) -> f64 {
        if let Some(gas_price) = &self.eip1559 {
            gas_price.max_priority_fee_per_gas
        } else {
            self.legacy
        }
    }

    // Bump gas price by factor.
    pub fn bump(self, factor: f64) -> Self {
        Self {
            legacy: self.legacy * factor,
            eip1559: self.eip1559.map(|x| x.bump(factor)),
        }
    }

    // Bump max gas price by factor.
    pub fn bump_cap(self, factor: f64) -> Self {
        Self {
            legacy: self.legacy * factor,
            eip1559: self.eip1559.map(|x| x.bump_cap(factor)),
        }
    }

    // Ceil gas price (since its defined as float).
    pub fn ceil(self) -> Self {
        Self {
            legacy: self.legacy.ceil(),
            eip1559: self.eip1559.map(|x| x.ceil()),
        }
    }

    // If current cap if higher then the input, set to input.
    pub fn limit_cap(self, cap: f64) -> Self {
        Self {
            legacy: self.legacy.min(cap),
            eip1559: self.eip1559.map(|x| x.limit_cap(cap)),
        }
    }
}

/// Gas price structure for 1559 transactions.
/// Contains base_fee_per_gas as an essential part of the gas price estimation.
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
    // Bump gas price by factor.
    pub fn bump(self, factor: f64) -> Self {
        Self {
            max_fee_per_gas: self.max_fee_per_gas * factor,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas * factor,
            ..self
        }
    }

    // Bump max gas price by factor.
    pub fn bump_cap(self, factor: f64) -> Self {
        Self {
            max_fee_per_gas: self.max_fee_per_gas * factor,
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

impl From<EstimatedGasPrice> for GasPrice {
    fn from(gas_price: EstimatedGasPrice) -> Self {
        if let Some(eip1559) = gas_price.eip1559 {
            (eip1559.max_fee_per_gas, eip1559.max_priority_fee_per_gas).into()
        } else {
            gas_price.legacy.into()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{EstimatedGasPrice, GasPrice1559};
    use assert_approx_eq::assert_approx_eq;

    #[test]
    fn cap_legacy() {
        //assert legacy is returned
        assert_approx_eq!(
            EstimatedGasPrice {
                legacy: 1.0,
                ..Default::default()
            }
            .cap(),
            1.0
        );
    }

    #[test]
    fn cap_eip1559() {
        //assert eip1559 is returned
        assert_approx_eq!(
            EstimatedGasPrice {
                eip1559: Some(GasPrice1559 {
                    max_fee_per_gas: 1.0,
                    ..Default::default()
                }),
                ..Default::default()
            }
            .cap(),
            1.0
        );
    }

    #[test]
    fn cap_legacy_and_eip1559() {
        //assert eip1559 is taken as default
        assert_approx_eq!(
            EstimatedGasPrice {
                legacy: 1.0,
                eip1559: Some(GasPrice1559 {
                    max_fee_per_gas: 2.0,
                    ..Default::default()
                }),
            }
            .cap(),
            2.0
        );
    }

    #[test]
    fn bump_and_ceil() {
        let gas_price = EstimatedGasPrice {
            legacy: 1.0,
            eip1559: Some(GasPrice1559 {
                max_fee_per_gas: 2.0,
                max_priority_fee_per_gas: 3.0,
                ..Default::default()
            }),
        };

        let gas_price_bumped = EstimatedGasPrice {
            legacy: 1.125,
            eip1559: Some(GasPrice1559 {
                max_fee_per_gas: 2.25,
                max_priority_fee_per_gas: 3.375,
                ..Default::default()
            }),
        };

        let gas_price_bumped_and_ceiled = EstimatedGasPrice {
            legacy: 2.0,
            eip1559: Some(GasPrice1559 {
                max_fee_per_gas: 3.0,
                max_priority_fee_per_gas: 4.0,
                ..Default::default()
            }),
        };

        assert_eq!(gas_price.bump(1.125), gas_price_bumped);
        assert_eq!(gas_price.bump(1.125).ceil(), gas_price_bumped_and_ceiled);
    }

    #[test]
    fn limit_cap_only_max_fee_capped() {
        let gas_price = EstimatedGasPrice {
            legacy: 10.0,
            eip1559: Some(GasPrice1559 {
                max_fee_per_gas: 5.0,
                max_priority_fee_per_gas: 3.0,
                ..Default::default()
            }),
        };

        let gas_price_capped = EstimatedGasPrice {
            legacy: 4.0,
            eip1559: Some(GasPrice1559 {
                max_fee_per_gas: 4.0,
                max_priority_fee_per_gas: 3.0,
                ..Default::default()
            }),
        };

        assert_eq!(gas_price.limit_cap(4.0), gas_price_capped);
    }

    #[test]
    fn limit_cap_max_fee_and_max_priority_capped() {
        let gas_price = EstimatedGasPrice {
            legacy: 10.0,
            eip1559: Some(GasPrice1559 {
                max_fee_per_gas: 5.0,
                max_priority_fee_per_gas: 3.0,
                ..Default::default()
            }),
        };

        let gas_price_capped = EstimatedGasPrice {
            legacy: 2.0,
            eip1559: Some(GasPrice1559 {
                max_fee_per_gas: 2.0,
                max_priority_fee_per_gas: 2.0,
                ..Default::default()
            }),
        };

        assert_eq!(gas_price.limit_cap(2.0), gas_price_capped);
    }

    #[test]
    fn estimate_legacy() {
        //assert legacy estimation is returned
        assert_approx_eq!(
            EstimatedGasPrice {
                legacy: 1.0,
                ..Default::default()
            }
            .effective_gas_price(),
            1.0
        );
    }

    #[test]
    fn estimate_eip1559() {
        //assert eip1559 estimation is returned
        assert_approx_eq!(
            EstimatedGasPrice {
                eip1559: Some(GasPrice1559 {
                    max_fee_per_gas: 10.0,
                    max_priority_fee_per_gas: 5.0,
                    base_fee_per_gas: 2.0
                }),
                ..Default::default()
            }
            .effective_gas_price(),
            7.0
        );

        assert_approx_eq!(
            EstimatedGasPrice {
                eip1559: Some(GasPrice1559 {
                    max_fee_per_gas: 10.0,
                    max_priority_fee_per_gas: 8.0,
                    base_fee_per_gas: 2.0
                }),
                ..Default::default()
            }
            .effective_gas_price(),
            10.0
        );

        assert_approx_eq!(
            EstimatedGasPrice {
                eip1559: Some(GasPrice1559 {
                    max_fee_per_gas: 10.0,
                    max_priority_fee_per_gas: 10.0,
                    base_fee_per_gas: 2.0
                }),
                ..Default::default()
            }
            .effective_gas_price(),
            10.0
        );
    }

    #[test]
    fn estimate_legacy_and_eip1559() {
        assert_approx_eq!(
            EstimatedGasPrice {
                eip1559: Some(GasPrice1559 {
                    max_fee_per_gas: 10.0,
                    max_priority_fee_per_gas: 5.0,
                    base_fee_per_gas: 2.0
                }),
                legacy: 8.0
            }
            .effective_gas_price(),
            7.0
        );
    }
}
