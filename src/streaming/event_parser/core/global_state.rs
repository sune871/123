use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use dashmap::DashMap;
use std::collections::BTreeSet;

const MAX_SLOTS: usize = 1000;
const CLEANUP_BATCH_SIZE: usize = 100;

/// Slot-based trader addresses, completely lock-free
#[derive(Default)]
struct SlotAddresses {
    /// Developer addresses for this slot
    dev_addresses: BTreeSet<Pubkey>,
    /// Bonk developer addresses for this slot  
    bonk_dev_addresses: BTreeSet<Pubkey>,
}

/// High-performance global state with lock-free slot-based storage
pub struct GlobalState {
    /// Slot -> trader addresses mapping (lock-free concurrent hashmap)
    slot_data: DashMap<u64, SlotAddresses>,
    /// Current slot count for capacity management
    slot_count: AtomicUsize,
    /// Generation counter to handle cleanup races
    generation: AtomicU64,
}

impl GlobalState {
    /// Create a new high-performance global state instance
    pub fn new() -> Self {
        Self {
            slot_data: DashMap::new(),
            slot_count: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
        }
    }

    /// Lock-free capacity management - cleanup old slots when limit exceeded
    fn maybe_cleanup(&self) {
        let current_count = self.slot_count.load(Ordering::Relaxed);
        if current_count <= MAX_SLOTS {
            return;
        }

        // Use CAS to ensure only one thread performs cleanup
        let gen = self.generation.load(Ordering::Relaxed);
        if self.generation.compare_exchange_weak(gen, gen + 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return; // Another thread is cleaning up
        }

        // Collect oldest slots (BTreeMap naturally orders by key)
        let mut slots_to_remove: Vec<u64> = self.slot_data.iter()
            .map(|entry| *entry.key())
            .collect();
        
        if slots_to_remove.len() <= MAX_SLOTS {
            return; // Race condition, already cleaned up
        }
        
        slots_to_remove.sort_unstable();
        slots_to_remove.truncate(CLEANUP_BATCH_SIZE);

        // Remove old slots atomically
        for slot in slots_to_remove {
            self.slot_data.remove(&slot);
            self.slot_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Add developer address for a specific slot (lock-free)
    pub fn add_dev_address(&self, slot: u64, address: Pubkey) {
        self.maybe_cleanup();
        
        self.slot_data.entry(slot)
            .and_modify(|addresses| {
                addresses.dev_addresses.insert(address);
            })
            .or_insert_with(|| {
                self.slot_count.fetch_add(1, Ordering::Relaxed);
                let mut slot_addr = SlotAddresses::default();
                slot_addr.dev_addresses.insert(address);
                slot_addr
            });
    }

    /// Add Bonk developer address for a specific slot (lock-free)
    pub fn add_bonk_dev_address(&self, slot: u64, address: Pubkey) {
        self.maybe_cleanup();
        
        self.slot_data.entry(slot)
            .and_modify(|addresses| {
                addresses.bonk_dev_addresses.insert(address);
            })
            .or_insert_with(|| {
                self.slot_count.fetch_add(1, Ordering::Relaxed);
                let mut slot_addr = SlotAddresses::default();
                slot_addr.bonk_dev_addresses.insert(address);
                slot_addr
            });
    }

    /// High-performance: Check if address is a developer address in specific slot (O(log m))
    pub fn is_dev_address_in_slot(&self, slot: u64, address: &Pubkey) -> bool {
        self.slot_data.get(&slot)
            .map(|entry| entry.dev_addresses.contains(address))
            .unwrap_or(false)
    }

    /// High-performance: Check if address is a Bonk developer address in specific slot (O(log m))
    pub fn is_bonk_dev_address_in_slot(&self, slot: u64, address: &Pubkey) -> bool {
        self.slot_data.get(&slot)
            .map(|entry| entry.bonk_dev_addresses.contains(address))
            .unwrap_or(false)
    }

    /// Check if address is a developer address in any slot (lock-free scan, slower)
    pub fn is_dev_address(&self, address: &Pubkey) -> bool {
        self.slot_data.iter().any(|entry| entry.dev_addresses.contains(address))
    }

    /// Check if address is a Bonk developer address in any slot (lock-free scan, slower)
    pub fn is_bonk_dev_address(&self, address: &Pubkey) -> bool {
        self.slot_data.iter().any(|entry| entry.bonk_dev_addresses.contains(address))
    }

    /// Get all developer addresses from all slots (lock-free aggregation)
    pub fn get_dev_addresses(&self) -> Vec<Pubkey> {
        let mut all_addresses = BTreeSet::new();
        for entry in self.slot_data.iter() {
            for addr in &entry.dev_addresses {
                all_addresses.insert(*addr);
            }
        }
        all_addresses.into_iter().collect()
    }

    /// Get all Bonk developer addresses from all slots (lock-free aggregation)
    pub fn get_bonk_dev_addresses(&self) -> Vec<Pubkey> {
        let mut all_addresses = BTreeSet::new();
        for entry in self.slot_data.iter() {
            for addr in &entry.bonk_dev_addresses {
                all_addresses.insert(*addr);
            }
        }
        all_addresses.into_iter().collect()
    }

    /// Get developer addresses for a specific slot
    pub fn get_dev_addresses_for_slot(&self, slot: u64) -> Vec<Pubkey> {
        self.slot_data.get(&slot)
            .map(|entry| entry.dev_addresses.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Get Bonk developer addresses for a specific slot
    pub fn get_bonk_dev_addresses_for_slot(&self, slot: u64) -> Vec<Pubkey> {
        self.slot_data.get(&slot)
            .map(|entry| entry.bonk_dev_addresses.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Get current slot count
    pub fn get_slot_count(&self) -> usize {
        self.slot_count.load(Ordering::Relaxed)
    }

    /// Clear all data (lock-free)
    pub fn clear_all_data(&self) {
        self.slot_data.clear();
        self.slot_count.store(0, Ordering::Relaxed);
        self.generation.store(0, Ordering::Relaxed);
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        Self::new()
    }
}

/// Global state instance
static GLOBAL_STATE: once_cell::sync::Lazy<GlobalState> =
    once_cell::sync::Lazy::new(GlobalState::new);

/// Get global state instance
pub fn get_global_state() -> &'static GlobalState {
    &GLOBAL_STATE
}

/// Convenience function: Add developer address for a specific slot
pub fn add_dev_address(slot: u64, address: Pubkey) {
    get_global_state().add_dev_address(slot, address);
}

/// Convenience function: Check if address is a developer address
pub fn is_dev_address(address: &Pubkey) -> bool {
    get_global_state().is_dev_address(address)
}

/// Convenience function: Add Bonk developer address for a specific slot
pub fn add_bonk_dev_address(slot: u64, address: Pubkey) {
    get_global_state().add_bonk_dev_address(slot, address);
}

/// Convenience function: Check if address is a Bonk developer address
pub fn is_bonk_dev_address(address: &Pubkey) -> bool {
    get_global_state().is_bonk_dev_address(address)
}

/// Convenience function: Get all developer addresses
pub fn get_dev_addresses() -> Vec<Pubkey> {
    get_global_state().get_dev_addresses()
}

/// Convenience function: Get all Bonk developer addresses
pub fn get_bonk_dev_addresses() -> Vec<Pubkey> {
    get_global_state().get_bonk_dev_addresses()
}

/// Convenience function: Get developer addresses for a specific slot
pub fn get_dev_addresses_for_slot(slot: u64) -> Vec<Pubkey> {
    get_global_state().get_dev_addresses_for_slot(slot)
}

/// Convenience function: Get Bonk developer addresses for a specific slot
pub fn get_bonk_dev_addresses_for_slot(slot: u64) -> Vec<Pubkey> {
    get_global_state().get_bonk_dev_addresses_for_slot(slot)
}

/// Convenience function: Get current slot count
pub fn get_slot_count() -> usize {
    get_global_state().get_slot_count()
}

/// High-performance: Check if address is a developer address in specific slot
pub fn is_dev_address_in_slot(slot: u64, address: &Pubkey) -> bool {
    get_global_state().is_dev_address_in_slot(slot, address)
}

/// High-performance: Check if address is a Bonk developer address in specific slot
pub fn is_bonk_dev_address_in_slot(slot: u64, address: &Pubkey) -> bool {
    get_global_state().is_bonk_dev_address_in_slot(slot, address)
}
