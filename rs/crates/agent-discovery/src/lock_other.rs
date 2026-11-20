use crate::agy::flock_held;

/// Asks for each lock with [`flock_held`]: there is no `/proc/locks` to read
/// without taking one.
pub(crate) fn new_lock_probe() -> Box<dyn Fn(&str) -> bool + Send + Sync> {
    Box::new(flock_held)
}
