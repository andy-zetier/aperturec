use crate::prelude::*;

use super::map::{Entry, EuclidMap};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EuclidSet {
    inner: EuclidMap<()>,
}

impl<R> FromIterator<R> for EuclidSet
where
    R: Into<Rect>,
{
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = R>,
    {
        EuclidSet {
            inner: EuclidMap::from_iter(iter.into_iter().map(|key| Entry {
                key: key.into(),
                value: (),
            })),
        }
    }
}

impl EuclidSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<R: Into<Rect>>(&mut self, key: R) -> Vec<Rect> {
        self.inner
            .insert(key, ())
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    }

    pub fn get(&self, point: Point) -> Option<Rect> {
        self.inner.get(point).map(|(key, _)| key)
    }

    pub fn get_at_origin(&self) -> Option<Rect> {
        self.get(Point::zero())
    }

    pub fn contains_point(&self, point: Point) -> bool {
        self.get(point).is_some()
    }

    pub fn contains<R: Into<Rect>>(&self, key: R) -> bool {
        let key = key.into();
        self.get(key.origin)
            .map(|rect| rect == key)
            .unwrap_or(false)
    }

    pub fn remove_at_point(&mut self, point: Point) -> Option<Rect> {
        self.inner.remove_at_point(point).map(|(key, _)| key)
    }

    pub fn remove_at_origin(&mut self) -> Option<Rect> {
        self.remove_at_point(Point::zero())
    }

    pub fn remove_all_overlaps<R: Into<Rect>>(&mut self, key: R) -> Vec<Rect> {
        self.inner
            .remove_all_overlaps(key)
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = Rect> + use<'_> {
        self.inner.keys()
    }
}

impl<'s> IntoIterator for &'s EuclidSet {
    type Item = Rect;
    type IntoIter = Box<dyn Iterator<Item = Rect> + 's>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use euclid::default::Point2D as Point;
    use euclid::default::Rect;

    // Utility function to create rectangles
    fn rect(x: usize, y: usize, width: usize, height: usize) -> Rect<usize> {
        Rect::new(Point::new(x, y), euclid::size2(width, height))
    }

    #[test]
    fn test_new_euclid_set_is_empty() {
        let set = EuclidSet::new();
        assert!(set.is_empty());
    }

    #[test]
    fn test_insert_rect() {
        let mut set = EuclidSet::new();
        let rect1 = rect(0, 0, 10, 10);
        let result = set.insert(rect1);
        assert!(result.is_empty());
        assert!(!set.is_empty());
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_insert_overlapping_rect() {
        let mut set = EuclidSet::new();
        let rect1 = rect(0, 0, 10, 10);
        let rect2 = rect(5, 5, 10, 10);
        set.insert(rect1);
        let result = set.insert(rect2);
        assert_eq!(result, vec![rect1]);
        assert_eq!(set.len(), 1); // Since rect2 overlaps rect1, only rect2 remains
    }

    #[test]
    fn test_get_rect_by_point() {
        let mut set = EuclidSet::new();
        let rect1 = rect(0, 0, 10, 10);
        set.insert(rect1);
        let point_inside = Point::new(5, 5);
        let point_outside = Point::new(15, 15);
        assert_eq!(set.get(point_inside), Some(rect1));
        assert_eq!(set.get(point_outside), None);
    }

    #[test]
    fn test_remove_at_point() {
        let mut set = EuclidSet::new();
        let rect1 = rect(0, 0, 10, 10);
        set.insert(rect1);
        let point_inside = Point::new(5, 5);
        let point_outside = Point::new(15, 15);
        assert_eq!(set.remove_at_point(point_inside), Some(rect1));
        assert_eq!(set.remove_at_point(point_outside), None);
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn test_remove_all_overlaps() {
        let mut set = EuclidSet::new();
        let rect1 = rect(0, 0, 10, 10);
        let rect2 = rect(5, 5, 10, 10);
        set.insert(rect1);
        let overlap = set.insert(rect2);
        assert_eq!(overlap[0], rect1);
        let target_rect = rect(2, 2, 5, 5);
        let removed = set.remove_all_overlaps(target_rect);
        assert_eq!(removed.len(), 1);
        assert!(set.is_empty());
    }

    #[test]
    fn test_iteration() {
        let mut set = EuclidSet::new();
        let rect1 = rect(0, 0, 10, 10);
        let rect2 = rect(20, 20, 10, 10);
        set.insert(rect1);
        set.insert(rect2);

        let mut iter = set.iter();
        assert_eq!(iter.next(), Some(rect1));
        assert_eq!(iter.next(), Some(rect2));
        assert_eq!(iter.next(), None);
    }
}
