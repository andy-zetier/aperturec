//! Continuity utility traits

use num::{PrimInt, Zero};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeBounds;

fn range_size<'i, I: PrimInt + 'i>(
    range: &impl RangeBounds<I>,
    min_max: impl Into<Option<(&'i I, &'i I)>>,
) -> usize {
    let min_max = min_max.into();
    let start = match range.start_bound() {
        std::ops::Bound::Included(&x) => x,
        std::ops::Bound::Excluded(&x) => x + I::one(),
        std::ops::Bound::Unbounded => {
            if let Some((&min, _)) = min_max {
                min
            } else {
                return 0;
            }
        }
    };
    let end = match range.end_bound() {
        std::ops::Bound::Included(&x) => x + I::one(),
        std::ops::Bound::Excluded(&x) => x,
        std::ops::Bound::Unbounded => {
            if let Some((_, &max)) = min_max {
                max + I::one()
            } else {
                return 0;
            }
        }
    };
    if end < start { I::zero() } else { end - start }
        .to_usize()
        .expect("range size overflow")
}

/// A trait representing a type that has a key.
pub trait Keyed {
    /// The type of the key.
    type Key;
}

/// A trait for types that have a minimum key.
pub trait Min: Keyed {
    /// Returns the minimum key, if it exists.
    fn min(&self) -> Option<&Self::Key>;
}

/// A trait for types that have a maximum key.
pub trait Max: Keyed {
    /// Returns the maximum key, if it exists.
    fn max(&self) -> Option<&Self::Key>;
}

/// A trait for types that can check if a range of keys is continuous.
pub trait Continuous: Keyed {
    /// Checks if the specified range of keys is continuous.
    ///
    /// # Arguments
    ///
    /// * `range` - The range of keys to check.
    ///
    /// # Returns
    ///
    /// * `true` if the range is continuous, `false` otherwise.
    fn is_continuous(&self, range: impl RangeBounds<Self::Key>) -> bool;
}

/// A trait for types that can check if they are strictly continuous.
pub trait StrictlyContinuous: Continuous {
    /// Checks if the entire range of keys is strictly continuous. No other values exist
    /// within this type that are not in the range.
    ///
    /// # Returns
    ///
    /// * `true` if the range is strictly continuous, `false` otherwise.
    fn is_strictly_continuous(&self) -> bool;
}

/// A trait for types that can check if they are continuous from zero.
pub trait ContinuousFromZero: Continuous {
    /// Checks if the range of keys from zero to the specified end is continuous.
    ///
    /// # Arguments
    ///
    /// * `end` - The end key of the range to check. This is an inclusive bound.
    ///
    /// # Returns
    ///
    /// * `true` if the range is continuous from zero, `false` otherwise.
    fn is_continuous_from_zero(&self, end: &Self::Key) -> bool;
}

/// A trait for types that can check if they are strictly continuous from zero.
pub trait StrictlyContinuousFromZero: ContinuousFromZero {
    /// Checks if the entire range of keys from zero is strictly continuous. No other values exist
    /// within this type that are not in the range.
    ///
    /// # Returns
    ///
    /// * `true` if the range is strictly continuous from zero, `false` otherwise.
    fn is_strictly_continuous_from_zero(&self) -> bool;
}

impl<C: Continuous> ContinuousFromZero for C
where
    C::Key: Zero,
{
    fn is_continuous_from_zero(&self, end: &Self::Key) -> bool {
        self.is_continuous(&Self::Key::zero()..=end)
    }
}

impl<C: StrictlyContinuous + Min> StrictlyContinuousFromZero for C
where
    C::Key: Zero + PartialEq,
{
    fn is_strictly_continuous_from_zero(&self) -> bool {
        self.min() == Some(&Self::Key::zero()) && self.is_strictly_continuous()
    }
}

impl<T: Continuous> StrictlyContinuous for T {
    fn is_strictly_continuous(&self) -> bool {
        self.is_continuous(..)
    }
}

impl<K, V> Keyed for BTreeMap<K, V> {
    type Key = K;
}

impl<K: PrimInt, V> Min for BTreeMap<K, V> {
    fn min(&self) -> Option<&Self::Key> {
        self.keys().min()
    }
}

impl<K: PrimInt, V> Max for BTreeMap<K, V> {
    fn max(&self) -> Option<&Self::Key> {
        self.keys().max()
    }
}

impl<K: PrimInt, V> Continuous for BTreeMap<K, V> {
    fn is_continuous(&self, range: impl RangeBounds<Self::Key>) -> bool {
        range_size(&range, Min::min(self).zip(Max::max(self))) == self.range(range).count()
    }
}

impl<K> Keyed for BTreeSet<K> {
    type Key = K;
}

impl<K: PrimInt> Min for BTreeSet<K> {
    fn min(&self) -> Option<&Self::Key> {
        self.iter().min()
    }
}

impl<K: PrimInt> Max for BTreeSet<K> {
    fn max(&self) -> Option<&Self::Key> {
        self.iter().max()
    }
}

impl<K: PrimInt> Continuous for BTreeSet<K> {
    fn is_continuous(&self, range: impl RangeBounds<Self::Key>) -> bool {
        range_size(&range, Min::min(self).zip(Max::max(self))) == self.range(range).count()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_range_size() {
        assert_eq!(range_size(&(1..=3), None), 3);
        assert_eq!(range_size(&(1..3), None), 2);
        assert_eq!(range_size(&(-1..1), None), 2);
        assert_eq!(range_size(&(..), (&0, &0)), 1);
        assert_eq!(range_size(&(..), (&0, &99)), 100);
    }

    #[test]
    fn test_btree_map_continuous() {
        let mut map = BTreeMap::new();
        map.insert(1, "a");
        map.insert(2, "b");
        map.insert(3, "c");

        assert!(!map.is_continuous(1..=4));
        assert!(map.is_continuous(1..4));
        assert!(map.is_continuous(..4));
        assert!(!map.is_continuous(..=4));
        assert!(map.is_continuous(..));
        assert!(map.is_strictly_continuous());

        map.insert(5, "d");
        assert!(!map.is_continuous(2..=5));
        assert!(!map.is_strictly_continuous());
    }

    #[test]
    fn test_btree_map_strictly_continuous() {
        let mut map = BTreeMap::new();
        assert!(!map.is_strictly_continuous_from_zero());
        map.insert(0, "a");
        assert!(map.is_strictly_continuous_from_zero());
        map.insert(1, "b");
        map.insert(2, "c");

        assert!(map.is_strictly_continuous_from_zero());
        map.remove(&0);
        assert!(!map.is_strictly_continuous_from_zero());
    }

    #[test]
    fn test_btree_set_continuous() {
        let mut set = BTreeSet::new();
        set.insert(1);
        set.insert(2);
        set.insert(3);

        assert!(!set.is_continuous(1..=4));
        assert!(set.is_continuous(1..4));
        assert!(set.is_continuous(..4));
        assert!(!set.is_continuous(..=4));
        assert!(set.is_continuous(..));
        assert!(set.is_strictly_continuous());

        set.insert(5);
        assert!(!set.is_continuous(2..=5));
        assert!(!set.is_strictly_continuous());
    }

    #[test]
    fn test_btree_set_strictly_continuous() {
        let mut set = BTreeSet::new();
        assert!(!set.is_strictly_continuous_from_zero());
        set.insert(0);
        assert!(set.is_strictly_continuous_from_zero());
        set.insert(1);
        set.insert(2);

        assert!(set.is_strictly_continuous_from_zero());
        set.remove(&0);
        assert!(!set.is_strictly_continuous_from_zero());
    }
}
