use borsh::{BorshSerialize, BorshDeserialize};

#[derive(BorshSerialize, BorshDeserialize, Debug, Clone)]
pub struct SwapBaseInput {
    pub amount_in: u64,
    pub min_amount_out: u64,
} 