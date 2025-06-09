use crate::prelude::*;

use std::{slice, vec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry<V> {
    pub(super) key: Rect,
    pub(super) value: V,
}

impl<'e, V> From<&'e Entry<V>> for (Rect, &'e V) {
    fn from(entry: &'e Entry<V>) -> (Rect, &'e V) {
        (entry.key, &entry.value)
    }
}

impl<'e, V> From<&'e mut Entry<V>> for (Rect, &'e mut V) {
    fn from(entry: &'e mut Entry<V>) -> (Rect, &'e mut V) {
        (entry.key, &mut entry.value)
    }
}

impl<V> From<Entry<V>> for (Rect, V) {
    fn from(entry: Entry<V>) -> (Rect, V) {
        (entry.key, entry.value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EuclidMap<V> {
    entries: Vec<Entry<V>>,
}

impl<V> Default for EuclidMap<V> {
    fn default() -> Self {
        EuclidMap { entries: vec![] }
    }
}

impl<V> EuclidMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<R: Into<Rect>>(&mut self, key: R, value: V) -> Vec<(Rect, V)> {
        let key = key.into();
        let existing = self.remove_all_overlaps(key);
        self.entries.push(Entry { key, value });
        self.entries
            .sort_by_key(|entry| <(_, _)>::from(entry.key.origin));
        existing
    }

    pub fn get(&self, point: Point) -> Option<(Rect, &V)> {
        self.entries
            .iter()
            .find(|entry| entry.key.contains(point))
            .map(Into::into)
    }

    pub fn get_mut(&mut self, point: Point) -> Option<(Rect, &mut V)> {
        self.entries
            .iter_mut()
            .find(|entry| entry.key.contains(point))
            .map(Into::into)
    }

    pub fn get_at_origin(&self) -> Option<(Rect, &V)> {
        self.get(Point::zero())
    }

    pub fn get_at_origin_mut(&mut self) -> Option<(Rect, &mut V)> {
        self.get_mut(Point::zero())
    }

    pub fn get_all_overlaps<R: Into<Rect>>(&self, key: R) -> impl Iterator<Item = (Rect, &V)> {
        let key = key.into();

        self.entries
            .iter()
            .filter(move |entry| entry.key.intersects(&key))
            .map(Into::into)
    }

    pub fn get_all_overlaps_mut<R: Into<Rect>>(
        &mut self,
        key: R,
    ) -> impl Iterator<Item = (Rect, &mut V)> {
        let key = key.into();

        self.entries
            .iter_mut()
            .filter(move |entry| entry.key.intersects(&key))
            .map(Into::into)
    }

    pub fn remove_at_point(&mut self, point: Point) -> Option<(Rect, V)> {
        self.entries
            .extract_if(.., |entry| entry.key.contains(point))
            .map(Into::into)
            .next()
    }

    pub fn remove_at_origin(&mut self) -> Option<(Rect, V)> {
        self.remove_at_point(Point::zero())
    }

    pub fn remove_all_overlaps<R: Into<Rect>>(&mut self, key: R) -> Vec<(Rect, V)> {
        let key = key.into();
        self.entries
            .extract_if(.., |entry: &mut Entry<V>| entry.key.intersects(&key))
            .map(Into::into)
            .collect()
    }

    pub fn pop(&mut self) -> Option<(Rect, V)> {
        self.entries.pop().map(Into::into)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn keys(&self) -> impl Iterator<Item = Rect> + use<'_, V> {
        self.entries.iter().map(|Entry { key, .. }| *key)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.entries.iter().map(|Entry { value, .. }| value)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.entries.iter_mut().map(|Entry { value, .. }| value)
    }

    pub fn iter(&self) -> Iter<'_, V> {
        self.into_iter()
    }

    pub fn iter_mut(&mut self) -> IterMut<'_, V> {
        self.into_iter()
    }
}

pub struct Iter<'m, V> {
    inner: slice::Iter<'m, Entry<V>>,
}

impl<'m, V> Iterator for Iter<'m, V> {
    type Item = (Rect, &'m V);
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|Entry { key, value }| (*key, value))
    }
}

impl<'m, V> IntoIterator for &'m EuclidMap<V> {
    type Item = (Rect, &'m V);
    type IntoIter = Iter<'m, V>;

    fn into_iter(self) -> Self::IntoIter {
        Iter {
            inner: self.entries.iter(),
        }
    }
}

pub struct IterMut<'m, V> {
    inner: slice::IterMut<'m, Entry<V>>,
}

impl<'m, V: 'm> Iterator for IterMut<'m, V> {
    type Item = (Rect, &'m mut V);
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|Entry { key, value }| (*key, value))
    }
}

impl<'m, V> IntoIterator for &'m mut EuclidMap<V> {
    type Item = (Rect, &'m mut V);
    type IntoIter = IterMut<'m, V>;

    fn into_iter(self) -> Self::IntoIter {
        IterMut {
            inner: self.entries.iter_mut(),
        }
    }
}

pub struct IntoIter<V> {
    inner: vec::IntoIter<Entry<V>>,
}

impl<V> Iterator for IntoIter<V> {
    type Item = (Rect, V);
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|Entry { key, value }| (key, value))
    }
}

impl<V> IntoIterator for EuclidMap<V> {
    type Item = (Rect, V);
    type IntoIter = IntoIter<V>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            inner: self.entries.into_iter(),
        }
    }
}

impl<R, V> FromIterator<(R, V)> for EuclidMap<V>
where
    R: Into<Rect>,
{
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (R, V)>,
    {
        EuclidMap::from_iter(iter.into_iter().map(|(key, value)| Entry {
            key: key.into(),
            value,
        }))
    }
}

impl<V> FromIterator<Entry<V>> for EuclidMap<V> {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = Entry<V>>,
    {
        let mut map = EuclidMap::new();
        for Entry { key, value } in iter {
            map.insert(key, value);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use euclid::rect;

    #[test]
    fn test_insert_and_get() {
        let mut map = EuclidMap::new();
        let rect1 = rect(0, 0, 10, 10);
        let point_inside = Point::new(5, 5);

        map.insert(rect1, "value1");

        let retrieved = map.get(point_inside);
        assert!(retrieved.is_some());
        assert_eq!(*retrieved.unwrap().1, "value1");
    }

    #[test]
    fn test_remove_at_point() {
        let mut map = EuclidMap::new();
        let rect1 = rect(0, 0, 10, 10);
        let point_inside = Point::new(5, 5);

        map.insert(rect1, "value1");

        let removed = map.remove_at_point(point_inside);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().1, "value1");

        // Ensure it's gone from the map.
        assert!(map.get(point_inside).is_none());
    }

    #[test]
    fn test_remove_all_overlaps() {
        let mut map = EuclidMap::new();
        let rect1 = rect(0, 0, 10, 10);
        let rect2 = rect(5, 5, 10, 10);

        map.insert(rect1, "value1");
        map.insert(rect2, "value2"); // Overlaps with rect1

        let removed = map.remove_all_overlaps(rect(2, 2, 20, 20));
        assert_eq!(removed.len(), 1); // Both should be removed
    }

    #[test]
    fn test_len_and_is_empty() {
        let mut map = EuclidMap::new();
        assert_eq!(map.len(), 0);
        assert!(map.is_empty());
        let rect1 = rect(0, 0, 10, 10);
        map.insert(rect1, "value1");
        assert_eq!(map.len(), 1);
        assert!(!map.is_empty());
        map.remove_at_point(Point::new(5, 5));
        assert_eq!(map.len(), 0);
        assert!(map.is_empty());
    }

    #[test]
    fn test_iteration() {
        let mut map = EuclidMap::new();
        let rect1 = rect(0, 0, 10, 10);
        let rect2 = rect(20, 20, 5, 5);

        map.insert(rect1, "value1");
        map.insert(rect2, "value2");

        // Test immutable iteration
        let collected: Vec<_> = map.iter().map(|(_, value)| *value).collect();
        assert_eq!(collected.len(), 2);
        assert!(collected.contains(&"value1"));
        assert!(collected.contains(&"value2"));

        // Test mutable iteration
        for (_, value) in map.iter_mut() {
            *value = "mutated";
        }

        let collected: Vec<_> = map.iter().map(|(_, value)| *value).collect();
        assert_eq!(collected, vec!["mutated", "mutated"]);
    }
}
