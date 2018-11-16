use actix::{Actor, SystemService};
use log::{info, warn};

use witnet_config::config::Config;
use witnet_util::timestamp::get_timestamp;

mod actor;
mod handlers;
mod messages;

/// Epoch id (starting from 0)
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Epoch(pub u64);

/// Possible errors when getting the current epoch
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum EpochManagerError {
    /// Epoch zero time is unknown
    UnknownEpochZero,
    /// Checkpoint period is unknown
    UnknownCheckpointPeriod,
    // Current time is unknown
    // (unused because get_timestamp() cannot fail)
    //UnknownTimestamp,
    /// Checkpoint zero is in the future
    CheckpointZeroInTheFuture,
    /// Overflow when calculating the epoch timestamp
    Overflow,
}

/// EpochManager actor
#[derive(Debug, Default)]
pub struct EpochManager {
    checkpoint_zero_timestamp: Option<i64>,
    checkpoints_period: Option<u16>,
}

/// Required trait for being able to retrieve EpochManager address from system registry
impl actix::Supervised for EpochManager {}

/// Required trait for being able to retrieve EpochManager address from system registry
impl SystemService for EpochManager {}

/// Auxiliary methods for EpochManager actor
impl EpochManager {
    /// Set the timestamp for the start of the epoch zero
    pub fn set_checkpoint_zero(&mut self, timestamp: i64) {
        self.checkpoint_zero_timestamp = Some(timestamp);
    }
    /// Set the checkpoint period (epoch duration)
    pub fn set_period(&mut self, mut period: u16) {
        if period == 0 {
            warn!("Setting the checkpoint period to the minimum value of 1 second");
            period = 1;
        }
        self.checkpoints_period = Some(period);
    }
    /// Calculate the last checkpoint (current epoch) at the supplied timestamp
    pub fn epoch_at(&self, timestamp: i64) -> messages::EpochResult<Epoch> {
        match (self.checkpoint_zero_timestamp, self.checkpoints_period) {
            (Some(zero), Some(period)) => {
                let elapsed = timestamp - zero;
                if elapsed < 0 {
                    Err(EpochManagerError::CheckpointZeroInTheFuture)
                } else {
                    let epoch = elapsed as u64 / u64::from(period);
                    Ok(Epoch(epoch))
                }
            }
            (None, _) => Err(EpochManagerError::UnknownEpochZero),
            (_, None) => Err(EpochManagerError::UnknownCheckpointPeriod),
        }
    }
    /// Calculate the last checkpoint (current epoch)
    pub fn current_epoch(&self) -> messages::EpochResult<Epoch> {
        let now = get_timestamp();
        self.epoch_at(now)
    }
    /// Calculate the timestamp for a checkpoint (the start of an epoch)
    pub fn epoch_timestamp(&self, epoch: Epoch) -> messages::EpochResult<i64> {
        match (self.checkpoint_zero_timestamp, self.checkpoints_period) {
            // Calculate (period * epoch + zero) with overflow checks
            (Some(zero), Some(period)) => u64::from(period)
                .checked_mul(epoch.0)
                .filter(|&x| x <= i64::max_value() as u64)
                .map(|x| x as i64)
                .and_then(|x| x.checked_add(zero))
                .ok_or(EpochManagerError::Overflow),
            (None, _) => Err(EpochManagerError::UnknownEpochZero),
            (_, None) => Err(EpochManagerError::UnknownCheckpointPeriod),
        }
    }
    /// Method to process the configuration received from the config manager
    fn process_config(&mut self, _ctx: &mut <Self as Actor>::Context, config: &Config) {
        self.set_checkpoint_zero(config.consensus_constants.checkpoint_zero_timestamp);
        self.set_period(config.consensus_constants.checkpoints_period);
        info!(
            "Checkpoint zero timestamp: {}",
            self.checkpoint_zero_timestamp.unwrap()
        );
    }
}
